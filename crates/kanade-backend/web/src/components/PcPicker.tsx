/**
 * PcPicker — the one shared way to put a `pc_id` into any page.
 *
 * Replaces the grab-bag of per-page inputs (a full `<select>` of every
 * agent on Logs, free-text boxes on Activity/Events/Run, a CSV box on
 * Exec) with a single async-search combobox. It queries
 * `GET /api/agents?q=&limit=50` as the operator types, so it stays flat
 * whether the fleet is 30 hosts or 3000 — the dropdown only ever holds
 * the typeahead candidates, never the whole table.
 *
 * Three modes cover the three things pages actually do with a pc_id:
 *
 *   - `single` (default) — pick exactly one *existing* host. Used by
 *     Logs / Inventory / Run. Free text can't be committed; only a
 *     candidate from the list sticks, which kills the silent-typo class
 *     of bug.
 *   - `multi` — pick several existing hosts, shown as removable chips.
 *     Used by Exec (was a comma-separated text box).
 *   - `filter` — a search/filter field that *also* accepts free text,
 *     because Activity/Events run a regex/substring match on the
 *     backend (`^PC001$`, partial ids, …). Candidates are offered as a
 *     convenience but typed text is honoured as-is.
 *
 * No Radix popover / cmdk dependency on purpose — a plain positioned
 * listbox keeps the SPA's lockfile untouched (see the known
 * `bun add`-migrates-to-strict-mode fragility).
 */

import { useQuery } from '@tanstack/react-query';
import { Check, Loader2, Search, X } from 'lucide-react';
import { useCallback, useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { useTranslation } from 'react-i18next';

import { apiFetch } from '@/lib/api';
import { useDebouncedValue } from '@/lib/hooks';
import type { AgentRow } from '@/lib/types';
import { cn } from '@/lib/utils';

type Mode = 'single' | 'multi' | 'filter';

interface BaseProps {
  id?: string;
  placeholder?: string;
  className?: string;
  disabled?: boolean;
}

type PcPickerProps =
  | (BaseProps & { mode?: 'single'; value: string; onChange: (value: string) => void })
  | (BaseProps & { mode: 'filter'; value: string; onChange: (value: string) => void })
  | (BaseProps & { mode: 'multi'; value: string[]; onChange: (value: string[]) => void });

const SEARCH_LIMIT = 50;
const SEARCH_DEBOUNCE_MS = 250;

export function PcPicker(props: PcPickerProps) {
  const mode: Mode = props.mode ?? 'single';
  const isMulti = mode === 'multi';
  const isSingle = mode === 'single';
  const { t } = useTranslation('common');

  const selected = isMulti ? (props.value as string[]) : [];
  const singleValue = isMulti ? '' : (props.value as string);

  const [query, setQuery] = useState('');
  const [open, setOpen] = useState(false);
  const [highlight, setHighlight] = useState(0);
  const rootRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  const debounced = useDebouncedValue(query.trim(), SEARCH_DEBOUNCE_MS);

  // single/filter: when the field is closed, mirror the committed value
  // back into the textbox so the page and the picker never disagree.
  useEffect(() => {
    if (!isMulti && !open) setQuery(singleValue ?? '');
  }, [isMulti, singleValue, open]);

  const commitClose = useCallback(() => {
    setOpen(false);
    // single: snap back to the committed value (free text isn't valid),
    // unless the operator explicitly cleared the box to drop the
    // selection. multi: clear the in-progress search term. filter:
    // leave query as-is — it's already the committed filter.
    if (isSingle) {
      if (query.trim() === '') (props.onChange as (v: string) => void)('');
      else setQuery(singleValue ?? '');
    } else if (isMulti) {
      setQuery('');
    }
  }, [isSingle, isMulti, singleValue, query, props.onChange]);

  // Close on an outside click — a plain listbox doesn't get Radix's
  // dismiss-on-blur for free. `commitClose` is memoized so this only
  // re-registers when it actually changes.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        commitClose();
      }
    };
    document.addEventListener('mousedown', onPointerDown);
    return () => document.removeEventListener('mousedown', onPointerDown);
  }, [open, commitClose]);

  const agentsQ = useQuery({
    enabled: open,
    queryKey: ['agents-search', debounced],
    queryFn: () =>
      apiFetch<AgentRow[]>(
        `/api/agents?limit=${SEARCH_LIMIT}${debounced ? `&q=${encodeURIComponent(debounced)}` : ''}`,
      ),
    staleTime: 30_000,
  });
  const options = agentsQ.data ?? [];

  // Keep the keyboard highlight in range (and on the first row for a
  // fresh search) as the candidate list shrinks or grows.
  useEffect(() => {
    setHighlight(0);
  }, [options.length]);

  function handleInput(next: string) {
    setQuery(next);
    setOpen(true);
    setHighlight(0);
    // filter mode commits every keystroke, matching the old free-text
    // box the page debounces on its own side.
    if (mode === 'filter') (props.onChange as (v: string) => void)(next);
  }

  function pick(pcId: string) {
    if (isMulti) {
      if (!selected.includes(pcId)) (props.onChange as (v: string[]) => void)([...selected, pcId]);
      setQuery('');
      setHighlight(0);
      inputRef.current?.focus(); // stay open to add more
    } else {
      (props.onChange as (v: string) => void)(pcId);
      setQuery(pcId);
      setOpen(false);
    }
  }

  function removeChip(pcId: string) {
    (props.onChange as (v: string[]) => void)(selected.filter((p) => p !== pcId));
  }

  function onKeyDown(e: KeyboardEvent<HTMLInputElement>) {
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        setOpen(true);
        setHighlight((h) => Math.min(h + 1, options.length - 1));
        break;
      case 'ArrowUp':
        e.preventDefault();
        setHighlight((h) => Math.max(h - 1, 0));
        break;
      case 'Enter':
        e.preventDefault();
        if (open && options[highlight]) pick(options[highlight].pc_id);
        else if (mode === 'filter') setOpen(false); // value already committed
        break;
      case 'Escape':
        if (open) {
          e.preventDefault();
          commitClose();
        }
        break;
      case 'Tab':
        // Let focus move on, but don't leave an orphaned dropdown open.
        if (open) commitClose();
        break;
      case 'Backspace':
        if (isMulti && query === '' && selected.length) {
          (props.onChange as (v: string[]) => void)(selected.slice(0, -1));
        }
        break;
    }
  }

  const showCheck = (pcId: string) =>
    isMulti ? selected.includes(pcId) : singleValue === pcId;

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
          selected.map((pc) => (
            <span
              key={pc}
              className="inline-flex items-center gap-1 rounded bg-muted/10 px-1.5 py-0.5"
            >
              <code className="text-xs">{pc}</code>
              <button
                type="button"
                aria-label={t('pcPicker.remove', { pcId: pc })}
                onClick={(e) => {
                  e.stopPropagation();
                  removeChip(pc);
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
          placeholder={props.placeholder ?? t('pcPicker.placeholder')}
          value={query}
          onChange={(e) => handleInput(e.target.value)}
          onFocus={() => setOpen(true)}
          onKeyDown={onKeyDown}
          role="combobox"
          aria-expanded={open}
          aria-autocomplete="list"
          aria-controls={props.id ? `${props.id}-listbox` : undefined}
          autoComplete="off"
        />
        {open && agentsQ.isFetching && (
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
            <li className="px-2 py-1.5 text-xs text-muted">
              {agentsQ.isLoading ? t('pcPicker.loading') : t('pcPicker.noMatch')}
            </li>
          ) : (
            options.map((a, i) => (
              <li
                key={a.pc_id}
                role="option"
                aria-selected={i === highlight}
                onMouseDown={(e) => {
                  e.preventDefault(); // keep focus; fire before blur
                  pick(a.pc_id);
                }}
                onMouseEnter={() => setHighlight(i)}
                className={cn(
                  'flex cursor-pointer items-center justify-between gap-2 rounded-sm px-2 py-1.5',
                  i === highlight && 'bg-muted/10',
                )}
              >
                <span className="flex min-w-0 flex-col">
                  <code className="truncate text-xs">{a.pc_id}</code>
                  {(a.hostname || a.os_family) && (
                    <span className="truncate text-[11px] text-muted">
                      {[a.hostname, a.os_family].filter(Boolean).join(' · ')}
                    </span>
                  )}
                </span>
                {showCheck(a.pc_id) && <Check className="size-4 shrink-0 text-accent" />}
              </li>
            ))
          )}
        </ul>
      )}
    </div>
  );
}
