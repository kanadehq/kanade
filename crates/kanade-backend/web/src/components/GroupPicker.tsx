/**
 * GroupPicker — the shared way to put a group `name` (or several) into
 * any page, the group-scope sibling of {@link PcPicker}.
 *
 * Where PcPicker async-searches `/api/agents` (the fleet can be
 * thousands of hosts), groups are few, so this fetches the whole
 * `/api/groups` overview once and filters client-side. The dropdown
 * offers existing groups as a typeahead.
 *
 * Two modes:
 *
 *   - `single` (default) — pick exactly one *existing* group. Used by
 *     the Config scope editor. Groups are *defined* on the Groups page
 *     (by assigning PCs), not minted as a side effect of editing a
 *     config override, so free text snaps back to the committed value
 *     on close.
 *   - `multi` — pick several groups as removable chips. Used by Exec's
 *     group-target field. Like PcPicker's multi mode it also accepts a
 *     comma/whitespace/newline-separated list, typed *or pasted*, so a
 *     bulk target list copied out of a doc splits into chips. Bulk
 *     tokens are checked against the known groups (the list is already
 *     loaded); names that don't exist are dropped and surfaced in an
 *     inline warning.
 *
 * Same dependency-free positioned listbox as PcPicker (no Radix / cmdk)
 * to keep the SPA lockfile untouched.
 */

import { useQuery } from '@tanstack/react-query';
import { AlertTriangle, Check, Loader2, Search, X } from 'lucide-react';
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ClipboardEvent,
  type KeyboardEvent,
} from 'react';
import { useTranslation } from 'react-i18next';

import { apiFetch } from '@/lib/api';
import { cn, splitTokens } from '@/lib/utils';

// Subset of the backend GroupsOverview (api/agent_groups.rs) we need.
type GroupSummary = { name: string };
type GroupsOverview = { groups: GroupSummary[] };

interface BaseProps {
  id?: string;
  placeholder?: string;
  className?: string;
  disabled?: boolean;
}

type GroupPickerProps =
  | (BaseProps & { mode?: 'single'; value: string; onChange: (value: string) => void })
  | (BaseProps & { mode: 'multi'; value: string[]; onChange: (value: string[]) => void });

export function GroupPicker(props: GroupPickerProps) {
  const isMulti = props.mode === 'multi';
  const { t } = useTranslation('common');

  const selected = isMulti ? (props.value as string[]) : [];
  const singleValue = isMulti ? '' : (props.value as string);

  const [query, setQuery] = useState('');
  const [open, setOpen] = useState(false);
  const [highlight, setHighlight] = useState(0);
  // multi: bulk tokens that don't match a known group, shown in an
  // inline warning until the next edit.
  const [rejected, setRejected] = useState<string[]>([]);
  const rootRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  // Latest committed selection, so the async fallback path merges onto
  // what's on screen now rather than a stale render-time snapshot.
  const selectedRef = useRef(selected);
  selectedRef.current = selected;

  // single: when closed, mirror the committed value back into the
  // textbox so the page and the picker never disagree.
  useEffect(() => {
    if (!isMulti && !open) setQuery(singleValue ?? '');
  }, [isMulti, singleValue, open]);

  const commitClose = useCallback(() => {
    setOpen(false);
    if (isMulti) {
      setQuery('');
    } else if (query.trim() === '') {
      // Clearing the box drops the selection…
      (props.onChange as (v: string) => void)('');
    } else {
      // …but typed free text snaps back (only an existing group counts).
      setQuery(singleValue ?? '');
    }
  }, [isMulti, query, singleValue, props.onChange]);

  // Read commitClose through a ref so the outside-click listener depends
  // only on `open` — commitClose closes over `query` and would otherwise
  // detach/reattach on every keystroke.
  const commitCloseRef = useRef(commitClose);
  commitCloseRef.current = commitClose;
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        commitCloseRef.current();
      }
    };
    document.addEventListener('mousedown', onPointerDown);
    return () => document.removeEventListener('mousedown', onPointerDown);
  }, [open]);

  const groupsQ = useQuery({
    enabled: open,
    queryKey: ['groups'],
    queryFn: () => apiFetch<GroupsOverview>('/api/groups'),
    staleTime: 30_000,
  });

  const q = query.trim().toLowerCase();
  const options = (groupsQ.data?.groups ?? []).filter(
    (g) => !q || g.name.toLowerCase().includes(q),
  );

  useEffect(() => {
    setHighlight(0);
  }, [options.length]);

  // multi: merge tokens into the selection, skipping duplicates.
  function addMany(names: string[]) {
    if (!isMulti) return;
    const current = selectedRef.current;
    const merged = [...current];
    for (const name of names) if (!merged.includes(name)) merged.push(name);
    if (merged.length !== current.length) {
      (props.onChange as (v: string[]) => void)(merged);
    }
  }

  // Existence-check bulk tokens against the known groups before they
  // become chips. The full list is already loaded once the field is
  // open; fall back to a fetch if not. Unknown names are dropped and
  // surfaced in the inline warning; a failed lookup commits as-is rather
  // than losing the operator's paste.
  async function addValidated(tokens: string[]) {
    const fresh = [...new Set(tokens)].filter((tk) => !selectedRef.current.includes(tk));
    if (!fresh.length) return;
    try {
      const data = groupsQ.data ?? (await apiFetch<GroupsOverview>('/api/groups'));
      const known = new Set(data.groups.map((g) => g.name));
      addMany(fresh.filter((tk) => known.has(tk)));
      setRejected(fresh.filter((tk) => !known.has(tk)));
    } catch {
      addMany(fresh);
    }
  }

  function handleInput(next: string) {
    setOpen(true);
    setHighlight(0);
    setRejected([]);
    // multi: a separator commits the completed tokens, leaving the
    // still-being-typed tail for the typeahead to keep filtering.
    if (isMulti && /[\s,]/.test(next)) {
      const endsWithSep = /[\s,]$/.test(next);
      const tokens = splitTokens(next);
      const tail = endsWithSep ? '' : (tokens.pop() ?? '');
      if (tokens.length) void addValidated(tokens);
      setQuery(tail);
      return;
    }
    setQuery(next);
  }

  // Paste is a complete action — split the whole clipboard at once.
  function handlePaste(e: ClipboardEvent<HTMLInputElement>) {
    if (!isMulti) return;
    const text = e.clipboardData.getData('text');
    if (!/[\s,]/.test(text)) return;
    e.preventDefault();
    setRejected([]);
    const tokens = splitTokens(text);
    if (tokens.length) void addValidated(tokens);
    setQuery('');
  }

  function pick(name: string) {
    if (isMulti) {
      addMany([name]);
      setQuery('');
      setHighlight(0);
      inputRef.current?.focus(); // stay open to add more
    } else {
      (props.onChange as (v: string) => void)(name);
      setQuery(name);
      setOpen(false);
    }
  }

  function removeChip(name: string) {
    (props.onChange as (v: string[]) => void)(selected.filter((g) => g !== name));
  }

  function onKeyDown(e: KeyboardEvent<HTMLInputElement>) {
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        setOpen(true);
        // Math.max(0, …) so an empty list doesn't park the highlight at
        // -1 (options.length - 1).
        setHighlight((h) => Math.max(0, Math.min(h + 1, options.length - 1)));
        break;
      case 'ArrowUp':
        e.preventDefault();
        setHighlight((h) => Math.max(h - 1, 0));
        break;
      case 'Enter':
        e.preventDefault();
        if (open && options[highlight]) pick(options[highlight].name);
        else if (isMulti && query.trim()) {
          // No candidate — existence-check the typed token (a typo is
          // dropped + warned), same path as a comma/paste.
          setRejected([]);
          void addValidated(splitTokens(query));
          setQuery('');
        }
        break;
      case 'Escape':
        if (open) {
          e.preventDefault();
          commitClose();
        }
        break;
      case 'Tab':
        if (open) commitClose();
        break;
      case 'Backspace':
        if (isMulti && query === '' && selected.length) {
          (props.onChange as (v: string[]) => void)(selected.slice(0, -1));
        }
        break;
    }
  }

  const showCheck = (name: string) =>
    isMulti ? selected.includes(name) : singleValue === name;

  return (
    <div ref={rootRef} className={cn('relative', props.className)}>
      <div
        className={cn(
          'flex min-h-9 w-full flex-wrap items-center gap-1 rounded-md border border-border bg-card px-2 py-1 text-sm shadow-sm transition-colors',
          'focus-within:outline-none focus-within:ring-2 focus-within:ring-accent',
          props.disabled && 'cursor-not-allowed opacity-50',
        )}
        onClick={() => !props.disabled && inputRef.current?.focus()}
      >
        <Search className="size-4 shrink-0 text-muted" />
        {isMulti &&
          selected.map((g) => (
            <span
              key={g}
              className="inline-flex items-center gap-1 rounded bg-muted/10 px-1.5 py-0.5"
            >
              <code className="text-xs">{g}</code>
              <button
                type="button"
                aria-label={t('groupPicker.remove', { group: g })}
                onClick={(e) => {
                  e.stopPropagation();
                  removeChip(g);
                }}
                className="text-muted hover:text-fg"
              >
                <X className="size-3" />
              </button>
            </span>
          ))}
        <input
          ref={inputRef}
          id={props.id}
          disabled={props.disabled}
          className="min-w-[6rem] flex-1 bg-transparent outline-none placeholder:text-muted disabled:cursor-not-allowed"
          placeholder={props.placeholder ?? t('groupPicker.placeholder')}
          value={query}
          onChange={(e) => handleInput(e.target.value)}
          onPaste={handlePaste}
          onFocus={() => setOpen(true)}
          onKeyDown={onKeyDown}
          role="combobox"
          aria-expanded={open}
          aria-autocomplete="list"
          aria-controls={props.id ? `${props.id}-listbox` : undefined}
          autoComplete="off"
        />
        {open && groupsQ.isFetching && (
          <Loader2 className="size-4 shrink-0 animate-spin text-muted" />
        )}
      </div>

      {open && (
        <ul
          id={props.id ? `${props.id}-listbox` : undefined}
          role="listbox"
          className="absolute z-50 mt-1 max-h-64 w-full overflow-auto rounded-md border border-border bg-bg p-1 shadow-md"
        >
          {options.length === 0 ? (
            <li
              className={cn('px-2 py-1.5 text-xs', groupsQ.isError ? 'text-danger' : 'text-muted')}
            >
              {groupsQ.isError
                ? t('groupPicker.error')
                : groupsQ.isLoading
                  ? t('groupPicker.loading')
                  : t('groupPicker.noMatch')}
            </li>
          ) : (
            options.map((g, i) => (
              <li
                key={g.name}
                role="option"
                aria-selected={i === highlight}
                onMouseDown={(e) => {
                  e.preventDefault(); // keep focus; fire before blur
                  pick(g.name);
                }}
                onMouseEnter={() => setHighlight(i)}
                className={cn(
                  'flex cursor-pointer items-center justify-between gap-2 rounded-sm px-2 py-1.5',
                  i === highlight && 'bg-muted/10',
                )}
              >
                <code className="truncate text-xs">{g.name}</code>
                {showCheck(g.name) && <Check className="size-4 shrink-0 text-accent" />}
              </li>
            ))
          )}
        </ul>
      )}

      {rejected.length > 0 && (
        <p className="mt-1 flex items-start gap-1.5 text-xs text-danger">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
          <span className="flex-1">{t('groupPicker.rejected', { names: rejected.join(', ') })}</span>
          <button
            type="button"
            aria-label={t('actions.clear')}
            onClick={() => setRejected([])}
            className="text-danger/70 hover:text-danger"
          >
            <X className="size-3" />
          </button>
        </p>
      )}
    </div>
  );
}
