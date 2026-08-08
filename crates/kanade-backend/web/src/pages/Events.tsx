import { useQuery } from '@tanstack/react-query';
import { ChevronDown, ChevronRight, Loader2 } from 'lucide-react';
import { Fragment, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useSearchParams } from 'react-router-dom';
import {
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Scatter,
  ScatterChart,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';

import { ErrorCard } from '@/components/ErrorCard';
import {
  OperationalTimeline,
  OP_TIMELINE_KINDS,
  type OpEvent,
  swimlaneWindow,
} from '@/components/OperationalTimeline';
import { PcPicker } from '@/components/PcPicker';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Label } from '@/components/ui/label';
import { Select } from '@/components/ui/select';
import { localInputToMs, msToLocalInput } from '@/lib/timeRange';
import { groupKinds, groupSources, shortLabel, type VocabGroup } from '@/lib/vocabGroups';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table';
import { apiFetch, apiFetchPaged } from '@/lib/api';
import { escapeRegExp, fmtIsoLocal } from '@/lib/utils';

type EventRow = {
  id: number;
  pc_id: string;
  at: string;
  kind: string;
  source: string;
  event_record_id: string | null;
  payload: unknown;
};

type ListResponse = { events: EventRow[] };
type KindsResponse = { kinds: string[] };
type SourcesResponse = { sources: string[] };

// Time-range presets come in four flavours:
//   rolling  — a fixed-length window ending now (now - ms → now).
//              Right for sub-day windows where "the last 6 hours"
//              is what the operator means literally.
//   calendar — N calendar days back to LOCAL midnight, counting
//              today as day 1: 1d = today 00:00→now, 2d = yesterday
//              00:00→now. An incident is remembered as "it happened
//              yesterday", not "it happened 31 hours ago", so the
//              day-scale presets snap to day boundaries.
//   all      — unbounded.
//   custom   — operator-picked absolute from/to.
type RangePreset =
  | { value: string; kind: 'rolling'; ms: number }
  | { value: string; kind: 'calendar'; days: number }
  | { value: string; kind: 'all' }
  | { value: string; kind: 'custom' };

const SINCE_PRESETS: RangePreset[] = [
  { value: '1h',     kind: 'rolling',  ms: 1 * 60 * 60 * 1000 },
  { value: '6h',     kind: 'rolling',  ms: 6 * 60 * 60 * 1000 },
  { value: '12h',    kind: 'rolling',  ms: 12 * 60 * 60 * 1000 },
  { value: '1d',     kind: 'calendar', days: 1 },
  { value: '2d',     kind: 'calendar', days: 2 },
  { value: '3d',     kind: 'calendar', days: 3 },
  { value: '7d',     kind: 'calendar', days: 7 },
  { value: '30d',    kind: 'calendar', days: 30 },
  { value: 'all',    kind: 'all' },
  { value: 'custom', kind: 'custom' },
];

// `2d` (yesterday 00:00 → now) rather than `1d`: at 00:05 a `1d`
// default would open the page on five minutes of data and read as
// "the events are gone". `2d` is the smallest calendar default that
// always covers a full day of history, and it supersets the old
// rolling-24h default it replaces.
//
// Note the asymmetry for the values that kept their names: `7d` and
// `30d` go the OTHER way and get NARROWER. Rolling `7d` reached back
// `now - 168h`; calendar `7d` reaches back to midnight six days ago,
// which is `now - 144h - (elapsed today)` — always at or later than
// the old bound, by up to 24h. At 09:00 the new `7d` is really 6.4
// days. That is the point (a day-scale window should start at a day
// boundary), but an operator with a `?since=7d` bookmark does lose
// the oldest rows they saw yesterday, so the preset labels spell the
// anchor out rather than just saying "last 7 days".
const DEFAULT_SINCE = '2d';

/**
 * Hydrate the `since` URL param, tolerating links from before the
 * calendar presets existed. Pre-#1073 URLs carry `24h`, which no
 * longer exists — map it onto `2d`, its nearest successor, so old
 * shared links keep resolving to a real window instead of silently
 * falling back to the default. Anything unrecognised (hand-edited
 * URL, preset removed later) also lands on the default.
 */
function normalizeSince(raw: string | null): string {
  if (!raw) return DEFAULT_SINCE;
  if (raw === '24h') return DEFAULT_SINCE;
  return SINCE_PRESETS.some((p) => p.value === raw) ? raw : DEFAULT_SINCE;
}

// The row-limit choices the <Select> offers, and the default. Single
// source of truth so the control and `normalizeLimit` can't drift.
export const LIMIT_OPTIONS = [50, 200, 1000, 5000] as const;
export const DEFAULT_LIMIT = 200;

/**
 * Hydrate the `limit` URL param, clamping to the four values the
 * <Select> actually offers (Issue #1077). The old `Number(raw) || 200`
 * accepted anything numeric-ish — `1e9`, `-5`, `200.5` all sailed
 * through to the request, which the backend 400s (its own limits are
 * `DEFAULT_LIMIT` 200 / `MAX_LIMIT` 5000). Worse, the URL-mirror effect
 * wrote the bad value straight back, so the error state survived a
 * reload and the blank <Select> (no matching option) offered no way
 * out. Same shape as `normalizeSince`: an out-of-range or unparseable
 * bound lands on the default, and the result is always a value the
 * <Select> can represent.
 */
export function normalizeLimit(raw: string | null): number {
  const n = Number(raw);
  return (LIMIT_OPTIONS as readonly number[]).includes(n) ? n : DEFAULT_LIMIT;
}

/**
 * Local midnight `days - 1` days ago — the lower bound of a calendar
 * preset. Local, not UTC: the operator's "today" is their wall
 * clock, and JST is +09:00, so a UTC-midnight anchor would cut the
 * day nine hours early.
 */
function calendarStart(days: number, now: Date): Date {
  const d = new Date(now);
  d.setHours(0, 0, 0, 0);
  d.setDate(d.getDate() - (days - 1));
  return d;
}

// <input type="datetime-local"> speaks local wall-clock strings
// ("2026-07-20T09:00"); the URL and the API speak UTC RFC3339.
// Converting at both boundaries keeps a shared custom-range link
// pointing at the same instant when it's opened in another timezone.
// The wall-clock ↔ epoch halves come from lib/timeRange, which the
// chart pages already use; only the epoch ↔ ISO hop is local, since
// those pages keep ranges as ms and this one has to put them on a
// URL and in a query param.
// Browser-side half of the same guard `msToBoundIso` enforces: keep
// the picker inside years the fixed-width ISO form can express, so the
// operator gets a native validation nudge instead of a silently
// rejected bound. `msToBoundIso` still has to check — `min`/`max` are
// advisory and a hand-edited URL bypasses them entirely.
const BOUND_INPUT_RANGE = { min: '1000-01-01T00:00', max: '9999-12-31T23:59' } as const;

function msToBoundIso(ms: number | null): string | null {
  if (ms === null) return null;
  const iso = new Date(ms).toISOString();
  // Reject anything that isn't the fixed-width 24-char form. Past year
  // 9999 (and before year 0) `toISOString` switches to the expanded
  // `+YYYYYY-…` / `-YYYYYY-…` notation, and that is not cosmetic: the
  // backend stores `at` with TEXT affinity, so `at >= ?` / `at < ?`
  // compare lexicographically. '+' and '-' sort below every digit, so
  // an expanded-year bound inverts the filter wholesale — the server
  // answers a "year 10000 onward" window with the entire table, 200 OK,
  // no warning. A local wall-clock of 9999-12-31T23:59 is already UTC
  // year 10000 in any negative-offset timezone, so this is reachable by
  // typing, not just by hand-editing the URL.
  return iso.length === 24 ? iso : null;
}

function isoToLocalInput(iso: string | null): string {
  return iso ? msToLocalInput(new Date(iso).getTime()) : '';
}

/**
 * Resolve a preset (plus the custom bounds) into the `from`/`to`
 * RFC3339 pair the API takes. `now` is injected rather than read
 * here so the caller controls when the window anchors — see the
 * #519 note at the useQuery call.
 */
function resolveEventsRange(
  since: string,
  customFromIso: string | null,
  customToIso: string | null,
  now: Date,
): { from: string | null; to: string | null } {
  const preset = SINCE_PRESETS.find((p) => p.value === since);
  if (!preset || preset.kind === 'all') return { from: null, to: null };
  if (preset.kind === 'custom') return { from: customFromIso, to: customToIso };
  if (preset.kind === 'rolling') {
    return { from: new Date(now.getTime() - preset.ms).toISOString(), to: null };
  }
  return { from: calendarStart(preset.days, now).toISOString(), to: null };
}

// Issue #391: payload keys the collectors are known to emit,
// offered as <datalist> suggestions for the generic payload
// filter. Free text stays allowed — the suggestions are a memory
// aid, not a schema.
const PAYLOAD_KEY_SUGGESTIONS = [
  'user', 'logon_type', 'session_id', 'sid', 'standby', 'from', 'wake_source',
] as const;

// Issue #391: tri-state chip cycle — off → include → exclude → off.
type ChipState = 'off' | 'include' | 'exclude';

function chipState(value: string, inc: string[], exc: string[]): ChipState {
  if (inc.includes(value)) return 'include';
  if (exc.includes(value)) return 'exclude';
  return 'off';
}

function cycleChip(
  value: string,
  inc: string[],
  exc: string[],
  setInc: (v: string[]) => void,
  setExc: (v: string[]) => void,
) {
  if (inc.includes(value)) {
    setInc(inc.filter((v) => v !== value));
    setExc([...exc, value]);
  } else if (exc.includes(value)) {
    setExc(exc.filter((v) => v !== value));
  } else {
    setInc([...inc, value]);
  }
}

// Issue #1342: a whole group's state, as a summary of its members'.
// `mixed` has no equivalent for a single chip — it exists so a folded
// group can admit that it is hiding a partial selection rather than
// rendering as untouched.
export type GroupChipState = ChipState | 'mixed';

export function groupState(values: string[], inc: string[], exc: string[]): GroupChipState {
  const first = chipState(values[0], inc, exc);
  return values.every((v) => chipState(v, inc, exc) === first) ? first : 'mixed';
}

/**
 * Cycle every member of a group together, following the same
 * off → include → exclude → off order a single chip uses so the two
 * controls stay predictable side by side.
 *
 * `mixed` enters at the start of that cycle (→ include all), which makes
 * the header a way to normalise a half-selected group rather than a
 * fourth state the operator has to reason about.
 *
 * Members are stripped from BOTH lists before being re-added: a group
 * that is half included and half excluded has entries in each, and
 * appending without removing would leave a value in both at once — a
 * state the backend resolves as excluded, silently contradicting the
 * green chip the operator would be looking at.
 */
export function cycleGroup(
  values: string[],
  inc: string[],
  exc: string[],
  setInc: (v: string[]) => void,
  setExc: (v: string[]) => void,
) {
  const state = groupState(values, inc, exc);
  const restInc = inc.filter((v) => !values.includes(v));
  const restExc = exc.filter((v) => !values.includes(v));
  if (state === 'include') {
    setInc(restInc);
    setExc([...restExc, ...values]);
  } else if (state === 'exclude') {
    setInc(restInc);
    setExc(restExc);
  } else {
    setInc([...restInc, ...values]);
    setExc(restExc);
  }
}

function splitCsv(s: string | null): string[] {
  // Mirror the backend's CSV hygiene (trim, drop blanks) and
  // dedupe, so a hand-edited / shared URL like `?kinds=logon,
  // logon , ,boot` hydrates the same chip state the server
  // filters by (CodeRabbit #394 minor).
  return s
    ? Array.from(new Set(s.split(',').map((v) => v.trim()).filter(Boolean)))
    : [];
}

function FilterChip({ label, state, onClick }: {
  label: string;
  state: ChipState;
  onClick: () => void;
}) {
  // State is conveyed by colour for sighted users; the aria-label
  // narrates it for screen readers (Gemini #394 medium). i18n keys
  // under `filters.chipStates.*`.
  const { t } = useTranslation('events');
  const cls =
    state === 'include'
      ? 'border-emerald-500/60 bg-emerald-500/15 text-emerald-300'
      : state === 'exclude'
        ? 'border-red-500/60 bg-red-500/10 text-red-400 line-through'
        : 'border-border text-muted hover:border-muted-foreground/50';
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={`${label}: ${t(`filters.chipStates.${state}`)}`}
      className={`rounded-full border px-2.5 py-0.5 text-xs cursor-pointer transition-colors ${cls}`}
    >
      {label}
    </button>
  );
}

/**
 * Issue #1342: one vocabulary's chip picker — the live selection over a
 * set of folded groups.
 *
 * Replaces a flat row of every distinct value. The vocabularies come
 * from the backend's DISTINCT lists and grow with every collector added
 * (23 kinds + 12 sources when this was written), so the flat form pushed
 * the results below the fold and buried the active selection among
 * dozens of inactive chips.
 *
 * Two rules carry the design:
 *
 *  - The selection renders ABOVE the groups and is never folded away.
 *    Hiding a chip is only acceptable while "what am I filtering by?"
 *    stays answerable without opening anything.
 *  - Group headers cycle their whole membership, so muting a family
 *    (the four `command_signature_*` kinds) is one click, not four.
 */
export function ChipGroupPicker({
  label,
  values,
  inc,
  exc,
  setInc,
  setExc,
  group,
}: {
  label: string;
  values: string[];
  inc: string[];
  exc: string[];
  setInc: (v: string[]) => void;
  setExc: (v: string[]) => void;
  group: (values: readonly string[]) => VocabGroup[];
}) {
  const { t } = useTranslation('events');
  const [expanded, setExpanded] = useState<string[]>([]);
  const groups = useMemo(() => group(values), [group, values]);

  // Ordered by the vocabulary, not by when each was clicked, so the
  // selection summary doesn't reshuffle under the pointer as it is
  // edited. Both lists are shown together — an exclusion is as much a
  // part of "what am I filtering by" as an inclusion.
  const selected = useMemo(
    () => values.filter((v) => inc.includes(v) || exc.includes(v)),
    [values, inc, exc],
  );

  // A selected value that is no longer in the vocabulary — a filter for
  // a kind the fleet has stopped emitting, or one hand-typed into the
  // URL. It still constrains the query, so it has to remain visible and
  // clearable; `values` alone would not surface it.
  // Deduped across the two lists, not just within each. `splitCsv`
  // dedupes `kinds` and `kinds_ex` separately, so `?kinds=retired&
  // kinds_ex=retired` puts the same value in both; concatenating them
  // would render it twice, and React would see two children with the
  // same key.
  const orphaned = useMemo(
    () => Array.from(new Set([...inc, ...exc])).filter((v) => !values.includes(v)),
    [values, inc, exc],
  );

  const groupLabel = (g: VocabGroup) =>
    g.labelKind === 'prefix' ? g.id : t(`filters.categories.${g.id}`);

  return (
    <div className="space-y-1">
      <div className="flex items-baseline gap-2">
        <Label className="mb-0">{label}</Label>
        {selected.length + orphaned.length > 0 && (
          <button
            type="button"
            onClick={() => {
              setInc([]);
              setExc([]);
            }}
            className="text-[11px] text-muted underline cursor-pointer hover:text-foreground"
          >
            {t('filters.clearSelection')}
          </button>
        )}
      </div>

      {/* The selection, always on screen. */}
      {selected.length + orphaned.length > 0 ? (
        <div className="flex flex-wrap gap-1.5 pb-1">
          {[...selected, ...orphaned].map((v) => (
            <FilterChip
              key={v}
              label={v}
              state={chipState(v, inc, exc)}
              onClick={() => cycleChip(v, inc, exc, setInc, setExc)}
            />
          ))}
        </div>
      ) : (
        <p className="pb-1 text-[11px] text-muted">{t('filters.noSelection')}</p>
      )}

      {/* The folded vocabulary. */}
      <div className="flex flex-wrap gap-1.5">
        {groups.map((g) => {
          const isOpen = expanded.includes(g.key);
          const state = groupState(g.values, inc, exc);
          // `mixed` deliberately reads as neither green nor red: the
          // group is partly selected, and painting it as one or the
          // other would misreport the members hidden inside it.
          const cls =
            state === 'include'
              ? 'border-emerald-500/60 bg-emerald-500/15 text-emerald-300'
              : state === 'exclude'
                ? 'border-red-500/60 bg-red-500/10 text-red-400'
                : state === 'mixed'
                  ? 'border-violet-500/60 bg-violet-500/10 text-violet-300'
                  : 'border-border text-muted hover:border-muted-foreground/50';
          return (
            // Collapsed groups flow inline and wrap, so the folded state is
            // genuinely compact — one `w-full` per group would spend a row
            // each and give back much of the vertical space the fold was
            // meant to reclaim. An OPEN group does claim a full row, so its
            // members sit under their own header instead of being visually
            // adopted by whichever group wrapped next to it.
            <div key={g.key} className={isOpen ? 'w-full' : ''}>
              <div className={`inline-flex items-center rounded-full border text-xs ${cls}`}>
                <button
                  type="button"
                  onClick={() =>
                    setExpanded((prev) =>
                      prev.includes(g.key) ? prev.filter((x) => x !== g.key) : [...prev, g.key],
                    )
                  }
                  aria-expanded={isOpen}
                  aria-label={t(isOpen ? 'filters.collapseGroup' : 'filters.expandGroup', {
                    group: groupLabel(g),
                  })}
                  className="cursor-pointer py-0.5 pl-2 pr-1"
                >
                  {isOpen ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
                </button>
                <button
                  type="button"
                  onClick={() => cycleGroup(g.values, inc, exc, setInc, setExc)}
                  aria-label={`${groupLabel(g)}: ${t(`filters.chipStates.${state}`)}`}
                  title={t('filters.chipHint')}
                  className="cursor-pointer py-0.5 pr-2.5 pl-0.5"
                >
                  {groupLabel(g)}
                  <span className="ml-1 opacity-60">{g.values.length}</span>
                </button>
              </div>
              {isOpen && (
                <div className="flex flex-wrap gap-1.5 py-1.5 pl-5">
                  {g.values.map((v) => (
                    <FilterChip
                      key={v}
                      // Inside a derived group the shared prefix is already
                      // on the header, so repeating it on every chip is noise
                      // — `winlog:Security` reads as `Security` under
                      // `winlog`. The stripping lives in the lib because it
                      // has to know the separator's length.
                      label={shortLabel(g, v)}
                      state={chipState(v, inc, exc)}
                      onClick={() => cycleChip(v, inc, exc, setInc, setExc)}
                    />
                  ))}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/**
 * Issue #1343: why a metadata search came back empty.
 *
 * `meta_any` matches through `agent_meta`, so a PC with no metadata
 * projected is excluded unconditionally. That makes a bare "no events
 * found" genuinely ambiguous, and the two readings call for opposite
 * responses:
 *
 *   - no machine carries that attribute value  → fix the search text;
 *   - the attribute was never populated at all → go populate it (or stop
 *     expecting this filter to work).
 *
 * Reading the first as a fact about the fleet — "nobody in Sales has any
 * events" — is the same class of mistake as #1073 / #1086, where a
 * filter's own limitation got read as a finding. So the empty state
 * answers with counts rather than leaving the operator to guess.
 *
 * Both queries are cheap and only run on the empty path: the PC count is
 * a `limit=1` request read from the count headers (no roster fetched),
 * and the key list is the same 60s-cached query the Agents page uses.
 */
function MetaSearchEmptyHint({ metaAny }: { metaAny: string }) {
  const { t } = useTranslation('events');

  // How many PCs the attribute search itself matches, independent of
  // any event. `X-Online-Count` + `X-Offline-Count` is the pre-LIMIT
  // matched set (see build_headers in api/agents.rs).
  const pcs = useQuery({
    queryKey: ['events-meta-any-pcs', metaAny],
    queryFn: () =>
      apiFetchPaged<unknown[]>(`/api/agents?limit=1&meta_any=${encodeURIComponent(metaAny)}`),
    enabled: metaAny !== '',
  });

  // Whether the fleet carries ANY metadata. Distinguishes "your search
  // matched nothing" from "this filter cannot work yet".
  const keys = useQuery({
    queryKey: ['agent-meta-keys'],
    queryFn: () => apiFetch<string[]>('/api/agents/meta-keys'),
    staleTime: 60_000,
  });

  // Say nothing until both answers are in. A half-formed diagnosis is
  // worse than none: this text exists to stop a misreading, and stating
  // "0 PCs" while the count is still loading would create one.
  if (!pcs.isSuccess || !keys.isSuccess) return null;

  const matched = (pcs.data.online ?? 0) + (pcs.data.offline ?? 0);
  if (matched > 0) {
    // The search is fine — those machines simply have no events in the
    // selected period.
    return <p className="mt-2 text-xs text-muted">{t('metaSearch.matchedNoEvents', { count: matched })}</p>;
  }
  return (
    <p className="mt-2 text-xs text-amber">
      {keys.data.length === 0
        ? t('metaSearch.noMetadataAtAll')
        : t('metaSearch.noPcMatched', { text: metaAny })}
    </p>
  );
}

// UAC split-token dedupe window (Issue #371). An interactive
// sign-in by an Administrators-group user writes TWO 4624s (full
// token + filtered token) microseconds apart, so a logon_type
// filtered view shows every human logon doubled. 2 s sits far
// above the observed pair distance (µs) and far below any genuine
// re-logon interval.
const DEDUPE_WINDOW_MS = 2_000;

const FILTER_DEBOUNCE_MS = 300;

function useDebouncedValue<T>(value: T, delay: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), delay);
    return () => clearTimeout(t);
  }, [value, delay]);
  return debounced;
}

// Map the kind vocabulary spelt out in #246 onto the four existing
// badge palettes so an operator skimming the timeline can chunk
// lifecycle (success/danger) vs informational (violet) vs neutral
// (default) without reading every cell.
// #496: cap the per-PC charts to the busiest hosts. At fleet scale a
// distinct-PC axis is unbounded — 3,000 PCs made the scatter a
// ~108,000px-tall SVG and the heatmap 3,000 rows x up-to-400 bucket
// cells (1.2M divs). Top-N by event count keeps the charts readable
// AND bounded; the table below still carries every event.
const CHART_MAX_PCS = 40;

// The four analysis views, shown one at a time via the tab bar (order =
// display order). 'operational' is the default. Kept as a const tuple so the
// type and the URL round-trip stay in sync.
const EVENTS_TABS = ['operational', 'chart', 'heatmap', 'table'] as const;
type EventsTab = (typeof EVENTS_TABS)[number];

// The operational kinds the swimlane reads, as a set for fast row filtering.
const OP_TIMELINE_KIND_SET = new Set(OP_TIMELINE_KINDS);

function topPcsByEventCount(
  events: EventRow[],
  max: number,
): { pcs: string[]; totalPcs: number } {
  const counts = new Map<string, number>();
  for (const e of events) counts.set(e.pc_id, (counts.get(e.pc_id) ?? 0) + 1);
  const ranked = Array.from(counts.entries()).sort(
    (a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1),
  );
  return {
    // Sorted for a stable axis order, same as the previous full list.
    pcs: ranked.slice(0, max).map(([pc]) => pc).sort(),
    totalPcs: ranked.length,
  };
}

// #496: stringify the payload only when the operator opens the
// <details> — eagerly serialising up to 5,000 payloads per render
// was a large hidden cost of the table.
function PayloadDetails({ payload }: { payload: unknown }) {
  const { t } = useTranslation('events');
  const [open, setOpen] = useState(false);
  // Sticky lazy cache: stringify on FIRST open only, then keep it so
  // repeated toggles don't re-pay the serialisation (PR #561 review,
  // claude — note a component-level useMemo would run during every
  // row's render and reintroduce the eager cost this component
  // exists to remove). Rows are keyed by event id, so a cached text
  // never outlives its payload.
  const [text, setText] = useState<string | null>(null);
  return (
    <details
      onToggle={(e) => {
        const isOpen = (e.target as HTMLDetailsElement).open;
        setOpen(isOpen);
        if (isOpen && text === null) {
          setText(JSON.stringify(payload, null, 2));
        }
      }}
    >
      <summary className="cursor-pointer text-muted text-xs">{t('payload.show')}</summary>
      {open && text !== null && (
        <pre className="text-xs whitespace-pre-wrap break-words mt-2 bg-muted/5 p-2 rounded max-h-96 overflow-y-auto">
          {text}
        </pre>
      )}
    </details>
  );
}

function kindVariant(kind: string): 'success' | 'amber' | 'danger' | 'violet' | 'default' {
  switch (kind) {
    case 'logon':
    case 'unlock':
    case 'boot':
    case 'resume':
    case 'agent_started':
      return 'success';
    case 'logoff':
    case 'lock':
    case 'shutdown':
    case 'sleep':
      return 'default';
    case 'unexpected_shutdown':
      return 'danger';
    case 'diagnostic':
    case 'agent_self_update':
    case 'wake_detail':
      return 'violet';
    default:
      return 'amber';
  }
}

// Hex equivalents of the Tailwind palette used in `kindVariant` —
// Recharts needs concrete fills, not class names. Kept aligned with
// the existing chart colours in AgentDetail.tsx so the operator's
// eye carries between pages.
const KIND_COLORS: Record<string, string> = {
  success: '#10b981', // emerald-500
  amber:   '#f59e0b', // amber-500
  danger:  '#ef4444', // red-500
  violet:  '#8b5cf6', // violet-500
  default: '#94a3b8', // slate-400
};
function kindColor(kind: string): string {
  return KIND_COLORS[kindVariant(kind)] ?? KIND_COLORS.default;
}

export function Events() {
  const { t } = useTranslation('events');
  const [search, setSearch] = useSearchParams();
  const [pcId, setPcId] = useState(search.get('pc') ?? '');
  // Issue #391: include/exclude chip selections. Legacy single-value
  // params (`kind`, `source`, `logon_type`) from pre-#391 shared
  // URLs migrate into the new shape on first render so old links
  // keep meaning the same thing.
  const [kindsInc, setKindsInc] = useState<string[]>(() => {
    const v = splitCsv(search.get('kinds'));
    const legacy = search.get('kind');
    return v.length === 0 && legacy ? [legacy] : v;
  });
  const [kindsExc, setKindsExc] = useState<string[]>(() => splitCsv(search.get('kinds_ex')));
  const [sourcesInc, setSourcesInc] = useState<string[]>(() => {
    const v = splitCsv(search.get('sources'));
    const legacy = search.get('source');
    return v.length === 0 && legacy ? [legacy] : v;
  });
  const [sourcesExc, setSourcesExc] = useState<string[]>(() => splitCsv(search.get('sources_ex')));
  // Issue #391: generic payload key=value filter (subsumes the old
  // logon_type select — `logon_type` URLs map onto it).
  const [payloadKey, setPayloadKey] = useState(() =>
    search.get('pkey') ?? (search.get('logon_type') ? 'logon_type' : ''));
  const [payloadValue, setPayloadValue] = useState(() =>
    search.get('pval') ?? search.get('logon_type') ?? '');
  // Issue #1343: global attribute search over the PC's operator-managed
  // `agent_meta` — the same box the Agents page calls the attribute
  // search (#1061). Narrows the MACHINES, not the events, so it composes
  // with every other filter rather than replacing any of them.
  const [metaAny, setMetaAny] = useState(() => search.get('meta_any') ?? '');
  // Dedupe defaults ON; only the opt-out lands in the URL.
  const [dedupe, setDedupe] = useState(search.get('dedupe') !== '0');
  const [since, setSince] = useState(() => normalizeSince(search.get('since')));
  // Custom absolute range. Held as datetime-local wall-clock strings
  // (what the inputs bind to); the URL and the API get UTC ISO.
  const [customFrom, setCustomFrom] = useState(() => isoToLocalInput(search.get('from')));
  const [customTo,   setCustomTo]   = useState(() => isoToLocalInput(search.get('to')));
  const [limit, setLimit] = useState(() => normalizeLimit(search.get('limit')));
  // Which analysis view is on screen. The four sections (operational
  // swimlanes, per-PC timeline, activity heatmap, raw event table) used to
  // stack vertically, so a fleet with many PCs buried the table under
  // screens of swimlanes. They're tabs now — only one renders at a time.
  const [tab, setTab] = useState<EventsTab>(
    EVENTS_TABS.includes(search.get('tab') as EventsTab)
      ? (search.get('tab') as EventsTab)
      : 'operational',
  );

  const dPcId         = useDebouncedValue(pcId,         FILTER_DEBOUNCE_MS);
  // Trimmed once, at the source, and used by every consumer — the API filter,
  // the shared URL, and the swimlane's pin. The backend filter is an exact
  // match (`pc_id = ?1`, obs_events.rs), so a pasted name with stray
  // whitespace matches nothing; with the trim applied in some places and not
  // others, the swimlane would pin a strip on the trimmed name and report the
  // host as having no events when the query never asked about it.
  const dPcIdTrimmed  = dPcId.trim();
  const dPayloadKey   = useDebouncedValue(payloadKey,   FILTER_DEBOUNCE_MS);
  const dPayloadValue = useDebouncedValue(payloadValue, FILTER_DEBOUNCE_MS);
  const dMetaAny      = useDebouncedValue(metaAny,      FILTER_DEBOUNCE_MS);
  // Trimmed once at the source, same as `dPcIdTrimmed` and for the same
  // reason: the backend reads a whitespace-only value as "no filter", so
  // an untrimmed value would put a filter on the URL that the query never
  // applied.
  const dMetaAnyTrimmed = dMetaAny.trim();
  // Debounced like the typed-text filters, so nudging one bound from
  // one complete value to another doesn't fire a fetch per segment.
  const dCustomFrom = useDebouncedValue(customFrom, FILTER_DEBOUNCE_MS);
  const dCustomTo   = useDebouncedValue(customTo,   FILTER_DEBOUNCE_MS);

  const customFromMs  = useMemo(() => localInputToMs(dCustomFrom), [dCustomFrom]);
  const customToMs    = useMemo(() => localInputToMs(dCustomTo),   [dCustomTo]);
  const customFromIso = useMemo(() => msToBoundIso(customFromMs),  [customFromMs]);
  const customToIso   = useMemo(() => msToBoundIso(customToMs),    [customToMs]);
  // A custom range needs BOTH bounds. Treating a missing bound as
  // "unbounded on that side" reads well but is a trap: `datetime-local`
  // reports `.value` as "" until every segment is filled, so clearing
  // the hour to retype it is indistinguishable from asking for an
  // open-ended range — and the page would answer by fetching the whole
  // retention window, fleet-wide, mid-edit. Blank bounds are a range
  // still being typed, not a range that means "everything".
  const customIncomplete =
    since === 'custom' && (customFromIso === null || customToIso === null);
  // Compared as instants, not as ISO text. String order only tracks
  // chronological order while both operands are the fixed-width form,
  // and reading `>=` on two strings invites someone to relax
  // `msToBoundIso` later without noticing this depends on it.
  const customInverted =
    since === 'custom'
    && customFromIso !== null
    && customToIso !== null
    && customFromMs !== null
    && customToMs !== null
    && customFromMs >= customToMs;
  // Neither state can produce a meaningful result set, so both gate
  // the query off — and, crucially, both have to be SHOWN (see the
  // results branch below). `enabled: false` only decides whether a
  // request goes out; on its own it leaves the page rendering the
  // ordinary "no events" card, which is the exact misread this is
  // meant to prevent.
  const customUnusable = customIncomplete || customInverted;

  // Calendar presets promise "today" / "yesterday", so the window has
  // to re-anchor the moment the local date changes. Nothing else would
  // do it: this query sets no `refetchInterval`, and a tab that stays
  // focused fires neither `refetchOnWindowFocus` nor a remount — so a
  // `1d` window opened at 23:50 would still be showing yesterday at
  // 00:10 while the picker reads "today". Switching the in-page tabs
  // doesn't help either; they don't remount the query.
  //
  // One timeout aimed at the next local midnight, rather than polling:
  // it costs a single timer and re-anchors exactly on the boundary.
  // `setHours(24, 0, 0, 0)` normalises to the start of the next day,
  // including across DST shifts.
  const [dayEpoch, setDayEpoch] = useState(0);
  useEffect(() => {
    const now = new Date();
    const nextMidnight = new Date(now);
    nextMidnight.setHours(24, 0, 0, 0);
    const id = setTimeout(
      () => setDayEpoch((n) => n + 1),
      // +1s of slack so the timer never lands a hair before midnight
      // and re-anchors to the day it was trying to leave.
      nextMidnight.getTime() - now.getTime() + 1_000,
    );
    return () => clearTimeout(id);
  }, [dayEpoch]);

  const isCalendarPreset =
    SINCE_PRESETS.find((p) => p.value === since)?.kind === 'calendar';

  // Mirror filters into the URL so a timeline drill-down link is
  // shareable / reload-safe (same shape as Logs). Uses the debounced
  // values for the typed-text inputs so a keystroke doesn't write a
  // partial URL on every change (Gemini #252 HIGH). `replace: true`
  // keeps these writes out of the back/forward stack, so polluting
  // history is a non-issue — no separate URL→state sync needed.
  useEffect(() => {
    const next = new URLSearchParams();
    if (dPcIdTrimmed) next.set('pc', dPcIdTrimmed);
    if (kindsInc.length)   next.set('kinds', kindsInc.join(','));
    if (kindsExc.length)   next.set('kinds_ex', kindsExc.join(','));
    if (sourcesInc.length) next.set('sources', sourcesInc.join(','));
    if (sourcesExc.length) next.set('sources_ex', sourcesExc.join(','));
    if (dPayloadKey)   next.set('pkey', dPayloadKey);
    if (dPayloadValue) next.set('pval', dPayloadValue);
    if (dMetaAnyTrimmed) next.set('meta_any', dMetaAnyTrimmed);
    if (!dedupe) next.set('dedupe', '0');
    if (since && since !== DEFAULT_SINCE) next.set('since', since);
    // Absolute bounds only belong in the URL while the custom preset
    // is selected — leaving them behind on a switch back to `7d`
    // would make the link look range-bound when it isn't.
    if (since === 'custom') {
      if (customFromIso) next.set('from', customFromIso);
      if (customToIso)   next.set('to',   customToIso);
    }
    if (limit !== DEFAULT_LIMIT)   next.set('limit', String(limit));
    if (tab !== 'operational') next.set('tab', tab);
    setSearch(next, { replace: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dPcIdTrimmed, kindsInc, kindsExc, sourcesInc, sourcesExc, dPayloadKey, dPayloadValue, dMetaAnyTrimmed, dedupe, since, customFromIso, customToIso, limit, tab]);

  const queryString = useMemo(() => {
    const sp = new URLSearchParams();
    sp.set('limit', String(limit));
    if (dPcIdTrimmed) sp.set('pc_id', dPcIdTrimmed);
    if (kindsInc.length)   sp.set('kinds', kindsInc.join(','));
    if (kindsExc.length)   sp.set('kinds_ex', kindsExc.join(','));
    if (sourcesInc.length) sp.set('sources', sourcesInc.join(','));
    if (sourcesExc.length) sp.set('sources_ex', sourcesExc.join(','));
    // Both halves of the payload pair must be present — a key with
    // no value (or vice versa) constrains nothing server-side.
    if (dPayloadKey && dPayloadValue) {
      sp.set('payload_key', dPayloadKey);
      sp.set('payload_value', dPayloadValue);
    }
    // Trimmed at the source like `pc_id`: the backend treats a
    // whitespace-only value as "no filter", so sending it unchanged
    // would make the URL claim a filter the query does not apply.
    if (dMetaAnyTrimmed) sp.set('meta_any', dMetaAnyTrimmed);
    return sp.toString();
  }, [dPcIdTrimmed, kindsInc, kindsExc, sourcesInc, sourcesExc, dPayloadKey, dPayloadValue, dMetaAnyTrimmed, limit]);

  const { data, error, isLoading, isFetching, dataUpdatedAt } = useQuery({
    // The preset key (not a computed ISO) partitions the cache per
    // window without invalidating on every millisecond tick (#519).
    // Custom ranges key on their absolute bounds, which are already
    // stable — but only while `custom` is selected. Keying on them
    // unconditionally would fragment the cache: pick a custom range,
    // switch back to `7d`, and the `7d` key now carries leftover
    // bounds it doesn't send, forcing a refetch of data already
    // cached under the original key.
    queryKey: [
      'obs_events',
      queryString,
      since,
      since === 'custom' ? customFromIso : null,
      since === 'custom' ? customToIso : null,
      // Bumped by the midnight timer above, so a calendar window
      // re-anchors on the date change instead of serving the cached
      // "today" that was computed yesterday.
      isCalendarPreset ? dayEpoch : 0,
    ],
    queryFn: () => {
      const sp = new URLSearchParams(queryString);
      // #519: the bounds are resolved HERE, not in render, so every
      // refetch re-anchors to now. A render-time ISO froze the window
      // at preset-pick time — an operator leaving the tab open saw
      // "last 1h" silently grow into "since whenever I opened this
      // page". Same reasoning covers the calendar presets, which have
      // to roll over when the clock passes midnight.
      const { from, to } = resolveEventsRange(since, customFromIso, customToIso, new Date());
      if (from) sp.set('from', from);
      if (to)   sp.set('to',   to);
      return apiFetch<ListResponse>(`/api/obs_events?${sp.toString()}`);
    },
    enabled: !customUnusable,
  });

  // The window the swimlane axis is drawn against — the same bounds the
  // fetch used, so the strip's edges are the period the operator picked.
  //
  // Anchored to `dataUpdatedAt` rather than a fresh `new Date()`: the latter
  // re-resolves on every render, which would jitter the axis (and rebuild the
  // memo) continuously. The fetch timestamp is stable between refetches and
  // is within milliseconds of the `now` the queryFn actually sent, and it
  // also gives the open right edge an honest meaning — "as of this data".
  const [windowFrom, windowTo] = useMemo<[string | undefined, string | undefined]>(() => {
    const anchor = dataUpdatedAt ? new Date(dataUpdatedAt) : new Date();
    const { from, to } = resolveEventsRange(since, customFromIso, customToIso, anchor);
    // No lower bound (`all`) → let the strip derive its own window.
    if (!from) return [undefined, undefined];
    return [from, to ?? anchor.toISOString()];
  }, [since, customFromIso, customToIso, dataUpdatedAt]);

  // Oldest instant the fetch actually reached back to, when `limit` truncated
  // it. The backend orders `at DESC, id DESC` and takes the newest N, so a
  // truncated response drops the OLD end of the selected period — the window
  // says two days, the data covers the last few hours. The swimlane has to
  // know, or `buildSpans`' carry-in extrapolates the oldest surviving event
  // across the entire uncovered stretch and paints a solid two-day claim.
  //
  // `>= limit` rather than `=== limit`: defensive, and the raw response is the
  // right thing to count — `visible` has been deduped, so it can sit below the
  // limit on a response that was in fact truncated.
  const coverageFrom = useMemo(() => {
    const rows = data?.events ?? [];
    if (rows.length < limit) return undefined;
    let lo = Infinity;
    for (const e of rows) {
      const ts = Date.parse(e.at);
      if (!Number.isNaN(ts) && ts < lo) lo = ts;
    }
    if (!Number.isFinite(lo)) return undefined;
    // A full page isn't proof of truncation: a period holding exactly `limit`
    // events returns a full page having fetched all of it. If the oldest row
    // reaches the start of the window, coverage is complete whatever the
    // count — without this the strip would hatch a region that is genuinely,
    // correctly empty and tell the operator to raise a limit that isn't
    // costing them anything.
    const floor = windowFrom ? Date.parse(windowFrom) : NaN;
    if (!Number.isNaN(floor) && lo <= floor) return undefined;
    return new Date(lo).toISOString();
  }, [data, limit, windowFrom]);

  // Chip vocabularies come from the backend's DISTINCT lists so the
  // operator picks from what actually exists in the data — no
  // hard-coded kind/source catalogue to maintain (Issue #391).
  const kindsQ = useQuery({
    queryKey: ['obs_events-kinds'],
    queryFn: () => apiFetch<KindsResponse>('/api/obs_events/kinds'),
    staleTime: 60_000,
  });
  const sourcesQ = useQuery({
    queryKey: ['obs_events-sources'],
    queryFn: () => apiFetch<SourcesResponse>('/api/obs_events/sources'),
    staleTime: 60_000,
  });

  // UAC split-token dedupe (Issue #371): collapse rows with the
  // same (pc, kind, payload.user) closer together than
  // DEDUPE_WINDOW_MS into the newest row of the group. View-only —
  // `obs_events` keeps both rows (faithful to the OS), and the
  // checkbox restores the raw view. Only active while the payload
  // filter targets logon_type (the forensic 4624 data set from
  // collect-winlog-logons-all / pre-#378 rows): that's the only
  // view where the split-token pairing reads as duplication rather
  // than data — Winlogon-sourced rows are 1:1 by construction.
  const dedupeApplicable = dPayloadKey === 'logon_type' && dPayloadValue !== '';
  const { visible, collapsed } = useMemo(() => {
    // Fallback array lives inside the memo so the `data === undefined`
    // render doesn't mint a fresh `[]` reference every pass, and the
    // dependency list stays exhaustive-deps-clean (Gemini #372 medium).
    const rows = data?.events ?? [];
    if (!dedupeApplicable || !dedupe) return { visible: rows, collapsed: 0 };
    const lastKept = new Map<string, number>();
    const visible: EventRow[] = [];
    let collapsed = 0;
    // Rows arrive `at DESC` from the API, so each group's newest
    // row is seen first and becomes the anchor its split-token
    // twin collapses into. The anchor only moves on a KEPT row, so
    // a slow drip of events spaced just inside the window can't
    // chain-collapse into one point.
    for (const e of rows) {
      const p = e.payload as { user?: unknown } | null;
      const user = typeof p?.user === 'string' ? p.user : '';
      const ts = Date.parse(e.at);
      if (Number.isNaN(ts)) {
        visible.push(e);
        continue;
      }
      const key = `${e.pc_id}|${e.kind}|${user}`;
      const anchor = lastKept.get(key);
      if (anchor !== undefined && anchor - ts < DEDUPE_WINDOW_MS) {
        collapsed++;
        continue;
      }
      lastKept.set(key, ts);
      visible.push(e);
    }
    return { visible, collapsed };
  }, [data, dedupeApplicable, dedupe]);

  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between">
        <h2 className="text-xl">{t('title')}</h2>
        <div className="flex items-baseline gap-2">
          {collapsed > 0 && (
            <span className="text-xs text-muted">
              {t('dedupeCollapsed', { count: collapsed })}
            </span>
          )}
          {/* No count while the custom range can't produce one — a
              "0 events" badge over an unfinished range reads as a
              finding about the fleet. */}
          {!customUnusable && (
            <Badge variant="violet">
              {isFetching && !isLoading
                ? t('countBadgeFetching', { count: visible.length })
                : t('countBadge', { count: visible.length })}
            </Badge>
          )}
        </div>
      </div>

      <Card>
        <CardContent className="space-y-3 p-4">
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-3">
            <div className="space-y-1">
              <Label htmlFor="ev-pc">{t('filters.pcId')}</Label>
              {/* filter mode keeps free text so the regex/substring backend filter still works */}
              <PcPicker
                mode="filter"
                id="ev-pc"
                placeholder={t('filters.placeholders.pcId')}
                value={pcId}
                onChange={setPcId}
              />
            </div>
            {/* Issue #1343: sits next to the PC filter because it answers
                the same question — WHICH MACHINES — just by attribute
                rather than by name. */}
            <div className="space-y-1">
              <Label htmlFor="ev-meta-any">{t('filters.metaAny')}</Label>
              <Input
                id="ev-meta-any"
                placeholder={t('filters.placeholders.metaAny')}
                value={metaAny}
                onChange={(e) => setMetaAny(e.target.value)}
              />
            </div>
            <div className="space-y-1">
              <Label>{t('filters.payload')}</Label>
              <div className="flex gap-1.5">
                <Input
                  id="ev-payload-key"
                  className="w-2/5"
                  list="ev-payload-keys"
                  placeholder={t('filters.placeholders.payloadKey')}
                  value={payloadKey}
                  onChange={(e) => setPayloadKey(e.target.value)}
                />
                <datalist id="ev-payload-keys">
                  {PAYLOAD_KEY_SUGGESTIONS.map((k) => <option key={k} value={k} />)}
                </datalist>
                <Input
                  id="ev-payload-value"
                  className="flex-1"
                  placeholder={t('filters.placeholders.payloadValue')}
                  value={payloadValue}
                  onChange={(e) => setPayloadValue(e.target.value)}
                />
              </div>
              {dedupeApplicable && (
                <div className="space-y-0.5 pt-1">
                  <label className="flex items-center gap-1.5 text-xs text-muted cursor-pointer">
                    <input
                      type="checkbox"
                      className="accent-violet-500"
                      checked={dedupe}
                      onChange={(e) => setDedupe(e.target.checked)}
                    />
                    {t('filters.dedupe')}
                  </label>
                  <p className="text-[11px] text-muted">{t('filters.forensicNote')}</p>
                </div>
              )}
            </div>
            <div className="space-y-1">
              <Label htmlFor="ev-since">{t('filters.since')}</Label>
              <Select id="ev-since" value={since} onChange={(e) => setSince(e.target.value)}>
                {SINCE_PRESETS.map((p) => (
                  <option key={p.value} value={p.value}>
                    {t(`filters.sincePresets.${p.value}`)}
                  </option>
                ))}
              </Select>
              {since === 'custom' && (
                <div className="space-y-1 pt-1">
                  <div className="space-y-0.5">
                    <Label htmlFor="ev-from" className="mb-0.5 text-xs font-normal normal-case tracking-normal text-muted">
                      {t('filters.customFrom')}
                    </Label>
                    <Input
                      id="ev-from"
                      type="datetime-local"
                      {...BOUND_INPUT_RANGE}
                      value={customFrom}
                      onChange={(e) => setCustomFrom(e.target.value)}
                    />
                  </div>
                  <div className="space-y-0.5">
                    <Label htmlFor="ev-to" className="mb-0.5 text-xs font-normal normal-case tracking-normal text-muted">
                      {t('filters.customTo')}
                    </Label>
                    <Input
                      id="ev-to"
                      type="datetime-local"
                      {...BOUND_INPUT_RANGE}
                      value={customTo}
                      onChange={(e) => setCustomTo(e.target.value)}
                    />
                  </div>
                  <p className="text-[11px] text-muted">{t('filters.customHint')}</p>
                  {customInverted && (
                    <p className="text-[11px] text-red-500">{t('filters.customInvalid')}</p>
                  )}
                </div>
              )}
            </div>
            <div className="space-y-1">
              <Label htmlFor="ev-limit">{t('filters.limit')}</Label>
              <Select
                id="ev-limit"
                value={String(limit)}
                onChange={(e) => setLimit(Number(e.target.value))}
              >
                {LIMIT_OPTIONS.map((n) => (
                  <option key={n} value={n}>{n}</option>
                ))}
              </Select>
            </div>
          </div>
          {/* Issue #391: tri-state chips — click cycles include
              (green) → exclude (red, struck) → off. Vocabulary is
              the backend's DISTINCT list, so new kinds/sources show
              up here without SPA changes.
              Issue #1342: folded into groups derived from that same
              list, with the live selection pinned above them. The URL
              still carries individual values (see the mirror effect
              above) — a category name there would change meaning
              whenever the grouping rules were edited, silently
              redefining links already shared. */}
          <ChipGroupPicker
            label={t('filters.kinds')}
            values={kindsQ.data?.kinds ?? []}
            inc={kindsInc}
            exc={kindsExc}
            setInc={setKindsInc}
            setExc={setKindsExc}
            group={groupKinds}
          />
          <ChipGroupPicker
            label={t('filters.sources')}
            values={sourcesQ.data?.sources ?? []}
            inc={sourcesInc}
            exc={sourcesExc}
            setInc={setSourcesInc}
            setExc={setSourcesExc}
            group={groupSources}
          />
        </CardContent>
      </Card>

      {customUnusable ? (
        /* Must come before the empty-set branch. A disabled query is
           `pending` + not `fetching`, which makes `isLoading` false and
           leaves `data` undefined — so without this the page would fall
           through to "no events found" and blame the fleet for what is
           really an unfinished range. */
        <Card>
          <CardHeader>
            <CardTitle>{t('customRange.title')}</CardTitle>
          </CardHeader>
          <CardContent className="text-muted">
            {customIncomplete ? t('customRange.incomplete') : t('customRange.inverted')}
          </CardContent>
        </Card>
      ) : isLoading ? (
        <div className="flex items-center gap-2 text-muted">
          <Loader2 className="size-4 animate-spin" />{t('loading')}
        </div>
      ) : error ? (
        <ErrorCard title={t('errorTitle')} error={error} />
      ) : visible.length === 0 ? (
        <Card>
          <CardHeader><CardTitle>{t('empty.title')}</CardTitle></CardHeader>
          <CardContent className="text-muted">
            {t('empty.body')}
            {/* #1343: a metadata search can come back empty for a reason
                that has nothing to do with the fleet's events. */}
            {dMetaAnyTrimmed && <MetaSearchEmptyHint metaAny={dMetaAnyTrimmed} />}
          </CardContent>
        </Card>
      ) : (
        <>
          {/* The four analysis views share one filtered set (`visible`);
              switching tabs just changes which one renders, so a many-PC
              fleet no longer buries the table under screens of swimlanes. */}
          <div
            role="tablist"
            aria-label={t('title')}
            className="inline-flex rounded-md border border-border bg-card text-sm overflow-hidden"
          >
            {EVENTS_TABS.map((k) => (
              <button
                key={k}
                type="button"
                role="tab"
                aria-selected={tab === k}
                // Roving tabindex: only the active tab sits in the Tab order
                // (WAI-ARIA Tabs pattern), matching the Config page tabs.
                tabIndex={tab === k ? 0 : -1}
                onClick={() => setTab(k)}
                // Arrow / Home / End move between tabs with automatic
                // activation, the other half of the WAI-ARIA Tabs pattern —
                // roving tabindex alone would otherwise strand keyboard users
                // on the active tab. Move DOM focus to the newly selected tab
                // so the roving index follows the selection.
                onKeyDown={(e) => {
                  const last = EVENTS_TABS.length - 1;
                  const idx = EVENTS_TABS.indexOf(k);
                  let next = idx;
                  if (e.key === 'ArrowRight') next = idx === last ? 0 : idx + 1;
                  else if (e.key === 'ArrowLeft') next = idx === 0 ? last : idx - 1;
                  else if (e.key === 'Home') next = 0;
                  else if (e.key === 'End') next = last;
                  else return;
                  e.preventDefault();
                  setTab(EVENTS_TABS[next]);
                  e.currentTarget.parentElement
                    ?.querySelectorAll<HTMLButtonElement>('[role="tab"]')
                    [next]?.focus();
                }}
                className={tab === k ? 'px-4 h-9 bg-accent/15 text-accent' : 'px-4 h-9 hover:bg-accent/5'}
              >
                {t(`tabs.${k}`)}
              </button>
            ))}
          </div>

          {tab === 'operational' && (
            <EventsOperational
              events={visible}
              windowFrom={windowFrom}
              windowTo={windowTo}
              coverageFrom={coverageFrom}
              limit={limit}
              // Debounced, so the pinned strip appears in step with the query
              // rather than one keystroke ahead of the data.
              pinnedPc={dPcIdTrimmed}
            />
          )}
          {tab === 'chart' && <EventsTimeline events={visible} />}
          {tab === 'heatmap' && <EventsHeatmap events={visible} />}
          {tab === 'table' && (
          <Table resizeKey="events">
          <TableHeader>
            <TableRow>
              <TableHead>{t('columns.when')}</TableHead>
              <TableHead>{t('columns.pcId')}</TableHead>
              <TableHead>{t('columns.kind')}</TableHead>
              <TableHead>{t('columns.source')}</TableHead>
              <TableHead>{t('columns.recordId')}</TableHead>
              <TableHead>{t('columns.payload')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {visible.map((e) => (
              <TableRow key={e.id}>
                <TableCell label={t('columns.when')} className="text-muted text-xs">{fmtIsoLocal(e.at)}</TableCell>
                <TableCell label={t('columns.pcId')}><code className="text-xs">{e.pc_id}</code></TableCell>
                <TableCell label={t('columns.kind')}>
                  {/* Click a row's kind badge to cycle the same include →
                      exclude → off filter as the chips above (e.g. mute the
                      noisy web_visit / presence rows). Wrapped in a real
                      <button> (not a bare onClick on the Badge span) so it
                      stays keyboard-reachable + screen-reader-narrated, like
                      FilterChip. */}
                  <button
                    type="button"
                    onClick={() => cycleChip(e.kind, kindsInc, kindsExc, setKindsInc, setKindsExc)}
                    aria-label={`${e.kind}: ${t(`filters.chipStates.${chipState(e.kind, kindsInc, kindsExc)}`)}`}
                    title={t('filters.chipHint')}
                    className="cursor-pointer rounded-full focus:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                  >
                    <Badge variant={kindVariant(e.kind)}>{e.kind}</Badge>
                  </button>
                </TableCell>
                <TableCell label={t('columns.source')}><code className="text-xs">{e.source}</code></TableCell>
                <TableCell label={t('columns.recordId')}>
                  {e.event_record_id
                    ? <code className="text-xs">{e.event_record_id}</code>
                    : <span className="text-muted text-xs">—</span>}
                </TableCell>
                <TableCell label={t('columns.payload')}>
                  <PayloadDetails payload={e.payload} />
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
          )}
        </>
      )}
    </div>
  );
}

/**
 * Per-PC operational swimlane section. Reconstructs power / session /
 * sleep intervals from the visible events and stacks one strip per PC
 * (capped to the busiest CHART_MAX_PCS, same as the other charts). All
 * strips share one time window so their axes line up. The span logic
 * lives in the shared `OperationalTimeline` component, so the Analytics
 * `op_timeline` widget renders identically.
 */
function EventsOperational({
  events,
  windowFrom,
  windowTo,
  coverageFrom,
  limit,
  pinnedPc,
}: {
  events: EventRow[];
  // Set when `limit` truncated the fetch: the oldest instant the data reaches.
  coverageFrom?: string;
  limit: number;
  // The selected period, resolved to bounds. When present these drive the
  // axis directly so "what I picked" and "what I see" are the same window.
  // Absent (the `all` preset has no lower bound) → fall back to the data.
  windowFrom?: string;
  windowTo?: string;
  // The PC filter, when the operator has named one. That host gets a strip
  // even with no events in the window — see the `pcs` memo.
  pinnedPc?: string;
}) {
  const { t } = useTranslation('events');

  // Only the operational kinds feed the swimlane; everything else (the
  // table's full event set) is ignored here.
  const opEvents = useMemo(
    () => events.filter((e) => OP_TIMELINE_KIND_SET.has(e.kind)),
    [events],
  );

  // Which strips to draw. Normally derived from the events, which means a PC
  // with nothing in the window simply isn't drawn — reasonable when the strip
  // list is "the busiest hosts", and wrong the moment an operator names one:
  // a host down long enough to have no events in the window then vanishes
  // instead of reading as unknown, which is the one question they were asking.
  //
  // So a named PC is pinned into the list even with zero events. Only a named
  // one: rendering an empty strip for every known host would bury the signal
  // under blank lanes for machines nobody asked about.
  //
  // And only one that exists (`pinExists`). Pinning on the typed string alone
  // would invent hosts: a typo, or any half-typed name on the way to a real
  // one, renders a full "we know nothing about this machine" strip for a
  // machine that was never in the fleet. Claiming ignorance about a real host
  // is the point of this change; claiming it about a fictional one is a new
  // way to mislead. The filter is an exact, case-sensitive match server-side
  // (`pc_id = ?1` in obs_events.rs), so there is no partial-match case to
  // reconcile — a name either identifies a host or identifies nothing.
  const found = useMemo(() => topPcsByEventCount(opEvents, CHART_MAX_PCS), [opEvents]);
  const pin = pinnedPc?.trim() || undefined;
  // What the heartbeat query asks about: the hosts with events, plus the
  // pinned name. Deliberately NOT derived from the pin decision below — that
  // decision consumes this query's result, so keying the query on it would
  // close a loop (pin → query → unpin → query → …).
  const queryPcs = useMemo(
    () => (pin && !found.pcs.includes(pin) ? [pin, ...found.pcs] : found.pcs),
    [found, pin],
  );

  // Heartbeats for the PCs on screen, so each strip can gate what it asserts
  // on whether its agent is still reporting. Without these the Events page
  // renders every strip solid to the live edge whatever the agent's state —
  // an offline host looks identical to a healthy one (#1086).
  //
  // One request for the rendered PCs rather than one per strip or the whole
  // fleet: `q` is a regex over pc_id / hostname, so an anchored alternation
  // fetches exactly the hosts in view — the same existence-check shape
  // `PcPicker` uses, down to `escapeRegExp` (a hostname with a regex
  // metacharacter would otherwise match the wrong rows, or none).
  //
  // No chunking here because `CHART_MAX_PCS` (40) is already the batch size
  // `PcPicker`'s `VALIDATE_CHUNK` picked to keep the URL-encoded pattern
  // under the ~2 KB request-line limit. That is exactly at the boundary, not
  // comfortably inside it: raising `CHART_MAX_PCS` means chunking this the
  // way `PcPicker` does, not just bumping the constant.
  // It doubles as the pin's existence check: the response only contains rows
  // that matched, so a pinned name absent from it is a name no agent has.
  const hbQ = useQuery({
    queryKey: ['events-op-heartbeats', queryPcs],
    queryFn: () =>
      apiFetch<{ pc_id: string; last_heartbeat: string | null }[]>(
        `/api/agents?limit=${queryPcs.length}&q=${encodeURIComponent(`^(${queryPcs.map(escapeRegExp).join('|')})$`)}`,
      ),
    enabled: queryPcs.length > 0,
    // Matches the Agents page cadence, so a strip stops claiming a live edge
    // within ~30s of the agent list marking the same host offline.
    refetchInterval: 30_000,
  });

  const heartbeats = useMemo(() => {
    const m = new Map<string, string | null>();
    for (const a of hbQ.data ?? []) m.set(a.pc_id, a.last_heartbeat);
    return m;
  }, [hbQ.data]);

  // Pin only a host that exists. Requires the query to have answered — while
  // it is in flight the pin stays off, so a real host appears a beat late
  // rather than a fictional one appearing immediately and vanishing.
  const pinExists = !!pin && hbQ.isSuccess && heartbeats.has(pin);

  const { pcs, totalPcs } = useMemo(() => {
    if (!pin || !pinExists || found.pcs.includes(pin)) return found;
    // Prepended, not appended: it's the host the operator asked for, so it
    // shouldn't sort below busier ones. The cap still holds.
    //
    // `totalPcs` only grows when the pin isn't in the data at all. A pin that
    // has events but fell below the cap is already counted, and adding one
    // would make the "showing N of M" note claim a host that doesn't exist.
    const inData = opEvents.some((e) => e.pc_id === pin);
    return {
      pcs: [pin, ...found.pcs].slice(0, CHART_MAX_PCS),
      totalPcs: found.totalPcs + (inData ? 0 : 1),
    };
  }, [opEvents, found, pin, pinExists]);

  // One shared window across all strips so every PC's lanes read on the same
  // axis. Pure and tested in OperationalTimeline.test.ts — this used to live
  // inline here, which is how the `all`-preset case (a pinned host with no
  // operational events losing its strip entirely) went unnoticed.
  const [from, to] = useMemo(
    () => swimlaneWindow(windowFrom, windowTo, pcs, opEvents, events),
    [events, opEvents, pcs, windowFrom, windowTo],
  );

  // Lane seeds: the newest event before the window, per PC and per lane.
  //
  // Without them a host that did not reboot inside the window reports no
  // power event at all, and the strip cannot tell "no winlog collector here"
  // from "stayed up the whole time" — it then re-synthesises power and
  // session from the sampler envelope and paints edge to edge (#1256). The
  // Analytics `op_timeline` query has always seeded itself; this is the same
  // footing for this page, so the two surfaces stop disagreeing about the
  // same host and window.
  //
  // Scoped to the PCs actually drawn (≤ CHART_MAX_PCS) and issued only once
  // they are known, so it is a bounded set of index seeks rather than a
  // fleet-wide walk back to whenever each host last rebooted. Separate query
  // key, so it caches on its own and never delays the main list.
  const seedPcs = useMemo(() => [...pcs].sort().join(','), [pcs]);
  // `swimlaneWindow` yields undefined bounds when there is nothing to span;
  // seeding a window that does not exist would be asking the backend to walk
  // history for a strip that will not be drawn.
  const seedBefore = from === undefined ? null : new Date(from).toISOString();
  const { data: seedData } = useQuery({
    queryKey: ['obs_events_lane_seeds', seedPcs, seedBefore],
    queryFn: () =>
      apiFetch<ListResponse>(
        `/api/obs_events/lane_seeds?pcs=${encodeURIComponent(seedPcs)}&before=${encodeURIComponent(
          seedBefore as string,
        )}`,
      ),
    enabled: seedPcs.length > 0 && seedBefore !== null,
    staleTime: 60_000,
  });

  // The one field the strip needs out of a payload: an `agent:startup`
  // event's OS boot time, which is what lets an outage be attributed to the
  // machine, the agent or the link rather than left as "cannot tell" (#1316).
  // Read defensively — `payload` is `unknown` here and arbitrary JSON on the
  // wire, so anything that isn't a number is simply absent.
  const bootTimeOf = (payload: unknown): number | undefined => {
    const v = (payload as { boot_time?: unknown } | null)?.boot_time;
    return typeof v === 'number' ? v : undefined;
  };

  // Group the kept PCs' events into the shape the strip wants.
  const byPc = useMemo(() => {
    const kept = new Set(pcs);
    const out = new Map<string, OpEvent[]>();
    // Seeds first so each lane's carry-in starts from the state the host was
    // already in. `buildSpans` sorts, so order here is for clarity only.
    for (const e of seedData?.events ?? []) {
      if (!kept.has(e.pc_id)) continue;
      const arr = out.get(e.pc_id);
      const seed = { at: e.at, kind: e.kind, source: e.source, bootTime: bootTimeOf(e.payload) };
      if (arr) arr.push(seed);
      else out.set(e.pc_id, [seed]);
    }
    for (const e of opEvents) {
      if (!kept.has(e.pc_id)) continue;
      const arr = out.get(e.pc_id);
      // `source` too: the strip needs it to tell "this host has no winlog
      // collector" from "this host did not reboot inside the window" (#1256).
      const row = { at: e.at, kind: e.kind, source: e.source, bootTime: bootTimeOf(e.payload) };
      if (arr) arr.push(row);
      else out.set(e.pc_id, [row]);
    }
    return out;
  }, [opEvents, pcs, seedData]);

  if (pcs.length === 0) return null;

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">
          {t('opTimeline.title')}
          {totalPcs > pcs.length && (
            <span className="ml-2 text-xs text-muted font-normal">
              {t('chartPcCap', { shown: pcs.length, total: totalPcs })}
            </span>
          )}
        </CardTitle>
        <p className="mt-1 text-muted text-xs">{t('opTimeline.subtitle')}</p>
        {/* Truncation is silent otherwise: the axis shows the full period, and
            without this the only clue is hatching an operator has to notice
            and interpret. Say plainly what's missing and how to get it. */}
        {coverageFrom && (
          <p className="mt-1 text-amber text-xs">
            {t('opTimeline.truncated', {
              limit,
              from: fmtIsoLocal(coverageFrom),
            })}
          </p>
        )}
      </CardHeader>
      <CardContent className="space-y-4 pt-0">
        {pcs.map((pc) => (
          <div key={pc} className="space-y-1">
            <code className="text-[11px] text-muted">{pc}</code>
            <OperationalTimeline
              events={byPc.get(pc) ?? []}
              from={from}
              to={to}
              coverageFrom={coverageFrom}
              // `undefined` while the heartbeat query is still in flight, so
              // the strip stays ungated rather than briefly hatching a
              // healthy agent's tail on every page load.
              lastHeartbeat={heartbeats.get(pc)}
            />
          </div>
        ))}
      </CardContent>
    </Card>
  );
}

/**
 * Per-PC timeline scatter — X axis is time, Y axis is PC name,
 * point colour encodes `kind`. One Scatter series per kind so the
 * legend doubles as a colour key and operators can pick a kind to
 * highlight just by hovering its legend entry.
 *
 * Sits above the existing detail table on the Events page; both
 * read from the same `rows` array so toggling filters at the top
 * narrows the chart and table together.
 */
function EventsTimeline({ events }: { events: EventRow[] }) {
  const { t } = useTranslation('events');

  // Stable PC list (sorted) drives the Y axis category domain. Doing
  // this once via useMemo so re-renders for tooltip hover don't
  // re-sort the list and confuse Recharts' axis caching. #496: capped
  // to the busiest CHART_MAX_PCS so the axis (and the SVG height
  // below) stays bounded at fleet scale.
  const { pcs, totalPcs } = useMemo(
    () => topPcsByEventCount(events, CHART_MAX_PCS),
    [events],
  );

  // Group points by kind so we can render one Scatter series per
  // kind (auto-coloured legend). Each point carries the original
  // event so the tooltip can render full context. Points for PCs
  // outside the top-N axis are dropped (they'd render off-axis).
  const byKind = useMemo(() => {
    const kept = new Set(pcs);
    const out: Record<string, Array<{ ts: number; pc: string; ev: EventRow }>> = {};
    for (const ev of events) {
      if (!kept.has(ev.pc_id)) continue;
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      (out[ev.kind] ??= []).push({ ts, pc: ev.pc_id, ev });
    }
    return out;
  }, [events, pcs]);

  // Height scales with PC count so a fleet-wide view doesn't squash
  // every row; floor at 200 keeps the single-PC case readable.
  const chartHeight = Math.max(200, 48 + pcs.length * 36);

  // Pre-compute the time window so the X axis is consistent across
  // re-renders. Recharts can auto-domain, but `type="number"`
  // requires an explicit domain to render the axis labels right.
  const [tMin, tMax] = useMemo(() => {
    // #496 / PR #561 review: derive the axis from the RENDERED
    // (top-N) PCs only, so a stray event on a dropped host can't
    // stretch the domain past what the operator sees.
    const kept = new Set(pcs);
    let lo = Infinity;
    let hi = -Infinity;
    for (const ev of events) {
      if (!kept.has(ev.pc_id)) continue;
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      if (ts < lo) lo = ts;
      if (ts > hi) hi = ts;
    }
    if (!isFinite(lo) || !isFinite(hi)) return [Date.now() - 60_000, Date.now()];
    // Pad 2% on each side so points don't hug the axis.
    const pad = Math.max(60_000, (hi - lo) * 0.02);
    return [lo - pad, hi + pad];
  }, [events, pcs]);

  // Decide X-axis tick format: short HH:mm when the window fits in a
  // day; switch to MM/DD HH:mm for multi-day ranges so the operator
  // can tell which Tuesday is which. useMemo keeps the function ref
  // stable across re-renders so Recharts doesn't trip its animation
  // diff on tooltip hover (Gemini #267 MEDIUM).
  const spanMs = tMax - tMin;
  const fmtTick = useMemo(() => {
    return (v: number) => {
      const d = new Date(v);
      if (spanMs > 24 * 60 * 60 * 1000) {
        return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`;
      }
      return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`;
    };
  }, [spanMs]);

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">
          {t('chart.title')}
          {totalPcs > pcs.length && (
            <span className="ml-2 text-xs text-muted font-normal">
              {t('chartPcCap', { shown: pcs.length, total: totalPcs })}
            </span>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent className="pt-0">
        <ResponsiveContainer width="100%" height={chartHeight}>
          <ScatterChart margin={{ top: 8, right: 24, bottom: 16, left: 0 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted/30" />
            <XAxis
              type="number"
              dataKey="ts"
              domain={[tMin, tMax]}
              tickFormatter={fmtTick}
              tick={{ fontSize: 11 }}
              stroke="currentColor"
              className="text-muted"
            />
            <YAxis
              type="category"
              dataKey="pc"
              // Explicit domain pins the row order even when a kind
              // series doesn't include every PC.
              domain={pcs}
              allowDuplicatedCategory={false}
              tick={{ fontSize: 11 }}
              width={120}
              stroke="currentColor"
              className="text-muted"
            />
            <Tooltip
              cursor={{ strokeDasharray: '3 3' }}
              content={({ active, payload }) => {
                if (!active || !payload?.length) return null;
                // Recharts can surface synthetic hover frames (e.g.
                // during cursor transitions) where `payload[0].payload`
                // exists but the embedded `ev` doesn't yet — bail out
                // before destructuring to avoid a tooltip-render crash
                // (Gemini #267 MEDIUM).
                const p = payload[0].payload as { ev?: EventRow } | undefined;
                if (!p?.ev) return null;
                const { ev } = p;
                return (
                  <div className="bg-card border border-border rounded px-2 py-1.5 text-xs shadow-md">
                    <div className="font-semibold">{ev.kind}</div>
                    <div className="text-muted">{fmtIsoLocal(ev.at)}</div>
                    <div><code>{ev.pc_id}</code> · <code>{ev.source}</code></div>
                  </div>
                );
              }}
            />
            <Legend wrapperStyle={{ fontSize: 11 }} />
            {Object.entries(byKind).map(([k, pts]) => (
              <Scatter key={k} name={k} data={pts} fill={kindColor(k)} />
            ))}
          </ScatterChart>
        </ResponsiveContainer>
      </CardContent>
    </Card>
  );
}

// Time-unit constants shared by the heatmap's bucket math. At module
// scope so the bucket-size useMemo + the continuous-buckets useMemo
// + the human-label code all read from the same source (Gemini #268
// HIGH cleanup).
const HOUR_MS = 60 * 60 * 1000;
const DAY_MS = 24 * HOUR_MS;

/**
 * PC × time-bucket heatmap. Where the scatter above shows each
 * individual event as a point, the heatmap aggregates into buckets
 * so longer windows (a week, a month) collapse into a readable
 * grid of "this PC, this hour: 12 events" cells. Useful for
 * spotting recurring patterns the scatter loses in cloud-density.
 *
 * Bucket size auto-adapts to the rendered window:
 *   ≤ 24h → 1 hour buckets  (max 24 columns)
 *   ≤ 7d  → 6 hour buckets  (max 28 columns)
 *   else  → 1 day buckets   (matches the operator's "since: 30d" pick)
 *
 * Colour intensity uses the same violet hue as `chart.kindColor`'s
 * informational bucket, scaled by `count / max(count)` so the
 * busiest cell is fully saturated. CSS grid + per-cell opacity keeps
 * the impl dependency-free (no heatmap library) — Recharts doesn't
 * ship a heatmap component.
 */
function EventsHeatmap({ events }: { events: EventRow[] }) {
  const { t } = useTranslation('events');

  // PC list — same shape (and same top-N cap, #496) as the scatter so
  // the two charts read in the same row order when stacked.
  const { pcs, totalPcs } = useMemo(
    () => topPcsByEventCount(events, CHART_MAX_PCS),
    [events],
  );

  // Determine the bucket size + the bucket alignment fn. Aligning to
  // wall-clock boundaries (top-of-hour, midnight) keeps the columns
  // labelled with round numbers operators can match against their
  // own log of "what was I doing at 14:00".
  const { bucketMs, alignBucket, fmtBucket } = useMemo(() => {
    // #496 / PR #561 review: span from the RENDERED (top-N) PCs only
    // — a stray week-old event on a dropped host would otherwise
    // flip the bucket size to daily for an hours-wide visible window.
    const kept = new Set(pcs);
    let lo = Infinity;
    let hi = -Infinity;
    for (const ev of events) {
      if (!kept.has(ev.pc_id)) continue;
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      if (ts < lo) lo = ts;
      if (ts > hi) hi = ts;
    }
    const span = isFinite(hi - lo) ? hi - lo : 0;
    if (span <= 24 * HOUR_MS) {
      return {
        bucketMs: HOUR_MS,
        alignBucket: (ts: number) => {
          const d = new Date(ts);
          d.setMinutes(0, 0, 0);
          return d.getTime();
        },
        fmtBucket: (ts: number) => {
          const d = new Date(ts);
          return `${d.getHours().toString().padStart(2, '0')}`;
        },
      };
    }
    if (span <= 7 * DAY_MS) {
      return {
        bucketMs: 6 * HOUR_MS,
        alignBucket: (ts: number) => {
          const d = new Date(ts);
          d.setHours(Math.floor(d.getHours() / 6) * 6, 0, 0, 0);
          return d.getTime();
        },
        fmtBucket: (ts: number) => {
          const d = new Date(ts);
          return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours().toString().padStart(2, '0')}`;
        },
      };
    }
    return {
      bucketMs: DAY_MS,
      alignBucket: (ts: number) => {
        const d = new Date(ts);
        d.setHours(0, 0, 0, 0);
        return d.getTime();
      },
      fmtBucket: (ts: number) => {
        const d = new Date(ts);
        return `${d.getMonth() + 1}/${d.getDate()}`;
      },
    };
  }, [events, pcs]);

  // (pc, bucket) → count + a CONTINUOUS bucket axis spanning lo..hi.
  //
  // Iterating from min to max in bucket-sized steps (instead of
  // collecting only the buckets that had events) is what makes the
  // X axis honest — idle stretches between events must show up as a
  // run of empty cells, otherwise non-consecutive bucket times
  // render side-by-side and recurrence patterns get misread (Gemini
  // #268 HIGH / CodeRabbit #268 Major).
  //
  // Date arithmetic via `setHours` / `setDate` (instead of raw
  // millisecond addition) keeps the iteration DST-safe — on a
  // forward-DST day a 24h span wall-clock-walks as 23 calendar
  // hours, and the setter handles that without dropping the last
  // bucket.
  const { buckets, counts, max } = useMemo(() => {
    const map = new Map<string, number>();
    const kept = new Set(pcs);
    let max = 0;
    let lo = Infinity;
    let hi = -Infinity;
    for (const ev of events) {
      // #496: dropped (beyond-top-N) PCs never render a cell, so
      // keeping them out of `max` keeps the alpha scale true to the
      // rows actually shown. The time window still derives from the
      // kept rows only, matching what the operator sees.
      if (!kept.has(ev.pc_id)) continue;
      const ts = Date.parse(ev.at);
      if (Number.isNaN(ts)) continue;
      if (ts < lo) lo = ts;
      if (ts > hi) hi = ts;
      const b = alignBucket(ts);
      const key = `${ev.pc_id}|${b}`;
      const next = (map.get(key) ?? 0) + 1;
      map.set(key, next);
      if (next > max) max = next;
    }
    const buckets: number[] = [];
    if (isFinite(lo) && isFinite(hi)) {
      const end = alignBucket(hi);
      let current = alignBucket(lo);
      // Hard cap — a misconfigured filter (e.g. 30-day window with
      // a daily bucket and a clock skew) shouldn't blow up the DOM.
      // 400 cells × 50 PCs still renders cleanly; past that the
      // operator should narrow the window.
      while (current <= end && buckets.length < 400) {
        buckets.push(current);
        const d = new Date(current);
        if (bucketMs === HOUR_MS) {
          d.setHours(d.getHours() + 1);
        } else if (bucketMs === DAY_MS) {
          d.setDate(d.getDate() + 1);
        } else {
          // 6h bucket: step in hours so DST shifts realign on the
          // next 6h boundary instead of drifting an hour ahead.
          d.setHours(d.getHours() + bucketMs / HOUR_MS);
        }
        current = d.getTime();
      }
    }
    return { buckets, counts: map, max };
  }, [events, pcs, alignBucket, bucketMs]);

  if (buckets.length === 0 || pcs.length === 0) return null;

  // Empty cell base + busiest-cell colour. Same violet the
  // informational bucket of `kindColor` uses, so the page's chart
  // palette stays consistent.
  const cellBase = '#8b5cf6'; // violet-500

  // Human label for the auto-picked bucket size — surfaced next to
  // the title so the operator knows whether they're reading "events
  // per hour" or "events per day" without inferring from the column
  // labels.
  const bucketLabel = bucketMs === HOUR_MS ? '1h' : bucketMs === DAY_MS ? '1d' : `${bucketMs / HOUR_MS}h`;

  return (
    <Card>
      <CardHeader className="pb-2">
        <CardTitle className="text-sm">
          {t('heatmap.title')}
          <span className="ml-2 text-xs text-muted font-normal">
            {t('heatmap.bucketHint', { bucket: bucketLabel })}
          </span>
          {totalPcs > pcs.length && (
            <span className="ml-2 text-xs text-muted font-normal">
              {t('chartPcCap', { shown: pcs.length, total: totalPcs })}
            </span>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent className="pt-0 overflow-x-auto">
        <div
          style={{
            display: 'grid',
            // First column = PC name label, then one column per bucket.
            // `auto` for the label so it sizes to the widest pc_id,
            // `minmax(18px, 1fr)` for cells so they stay visible on a
            // wide window but tile cleanly on a narrow one.
            gridTemplateColumns: `auto repeat(${buckets.length}, minmax(18px, 1fr))`,
            gap: '2px',
            fontSize: '10px',
          }}
        >
          {/* header row: empty corner + bucket labels */}
          <div />
          {buckets.map((b) => (
            <div key={b} className="text-muted text-center leading-none pt-1" title={fmtIsoLocal(new Date(b).toISOString())}>
              {fmtBucket(b)}
            </div>
          ))}
          {/* per-PC rows: pc_id label + cells. Fragment shorthand
              `<>` doesn't accept a `key`, so use the explicit
              React.Fragment form to silence the reconciler warning. */}
          {pcs.map((pc) => (
            <Fragment key={pc}>
              <div className="text-muted pr-2 self-center">
                <code className="text-[10px]">{pc}</code>
              </div>
              {buckets.map((b) => {
                const c = counts.get(`${pc}|${b}`) ?? 0;
                // Linear scale (count/max) saturates fast on long-tail
                // event distributions, but for the 0..~20 range typical
                // of a fleet's daily logon/sleep tally it reads well
                // enough that the log-scale alternative isn't worth
                // the explanation overhead.
                const alpha = c === 0 ? 0 : 0.15 + 0.85 * (c / max);
                return (
                  <div
                    key={`${pc}-${b}`}
                    className="rounded-sm h-5"
                    style={{
                      backgroundColor: c === 0
                        ? 'rgba(148, 163, 184, 0.08)' // slate-400 very light = "no events"
                        : `rgba(139, 92, 246, ${alpha})`,
                    }}
                    title={`${pc} · ${fmtIsoLocal(new Date(b).toISOString())}: ${t('heatmap.cellTooltip', { count: c })}`}
                  />
                );
              })}
            </Fragment>
          ))}
        </div>
        {/* Legend strip: 0 → max, gradient swatch */}
        <div className="mt-3 flex items-center gap-2 text-xs text-muted">
          <span>0</span>
          <div
            className="h-3 w-32 rounded-sm"
            style={{
              background: `linear-gradient(to right, rgba(148,163,184,0.08), ${cellBase})`,
            }}
          />
          <span>{max}</span>
        </div>
      </CardContent>
    </Card>
  );
}
