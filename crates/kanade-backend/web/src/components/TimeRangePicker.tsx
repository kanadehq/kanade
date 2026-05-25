import { useCallback } from 'react';

import { Input } from '@/components/ui/input';
import { Select } from '@/components/ui/select';
import {
  type PresetOption,
  type RangeValue,
  type StepOption,
  localInputToMs,
  msToLocalInput,
  resolveRange,
} from '@/lib/timeRange';

export interface TimeRangePickerTexts {
  /** "Range" label preceding the preset Select. */
  rangeLabel: string;
  /** "Mode" label preceding the preset/custom toggle. */
  modeLabel: string;
  modePreset: string;
  modeCustom: string;
  fromLabel: string;
  toLabel: string;
  stepLabel: string;
  /** Shown under the inputs when from >= to. */
  invalidHint: string;
}

interface Props {
  value: RangeValue;
  onChange: (next: RangeValue) => void;
  presets: readonly PresetOption[];
  stepOptions: readonly StepOption[];
  texts: TimeRangePickerTexts;
}

/** Range selector with two modes:
 *
 *  - **preset**: a one-click "Last 1h / 6h / …" Select. The chart's
 *    right edge advances with the wall clock via `useResolvedRange`.
 *  - **custom**: two `<input type="datetime-local">` + a `step` Select.
 *    The window is frozen at user-picked absolute times — the use case
 *    is investigating a past incident, where chasing "now" would
 *    silently scroll the bug off the chart.
 *
 *  Switching modes preserves intent rather than blanking inputs:
 *  preset → custom seeds the inputs from the currently-resolved
 *  preset window (so the user can nudge endpoints instead of typing
 *  from scratch); custom → preset jumps back to the first preset
 *  (the simpler, predictable default — no hidden remembered state).
 */
export function TimeRangePicker({
  value,
  onChange,
  presets,
  stepOptions,
  texts,
}: Props) {
  const setMode = useCallback(
    (mode: 'preset' | 'custom') => {
      if (mode === value.mode) return;
      if (mode === 'preset') {
        const first = presets[0];
        if (!first) return;
        onChange({ mode: 'preset', presetKey: first.key });
        return;
      }
      // preset → custom: seed inputs from the currently resolved
      // window so the user starts from familiar endpoints.
      const resolved = resolveRange(value, presets, Date.now());
      const fallbackStep = stepOptions[0];
      const step = stepOptions.find((s) => s.value === resolved.step) ?? fallbackStep;
      if (!step) return;
      onChange({
        mode: 'custom',
        fromMs: new Date(resolved.fromIso).getTime(),
        toMs: new Date(resolved.toIso).getTime(),
        step: step.value,
        stepSecs: step.secs,
      });
    },
    [value, presets, stepOptions, onChange],
  );

  const updateCustom = useCallback(
    (patch: Partial<Extract<RangeValue, { mode: 'custom' }>>) => {
      if (value.mode !== 'custom') return;
      onChange({ ...value, ...patch });
    },
    [value, onChange],
  );

  const customInvalid =
    value.mode === 'custom' && value.fromMs >= value.toMs;

  return (
    <div className="flex flex-wrap items-center gap-2">
      <span className="text-xs text-muted">{texts.modeLabel}</span>
      <div
        role="tablist"
        aria-label={texts.modeLabel}
        className="inline-flex rounded-md border border-border bg-card text-xs overflow-hidden"
      >
        <button
          type="button"
          role="tab"
          aria-selected={value.mode === 'preset'}
          onClick={() => setMode('preset')}
          className={
            value.mode === 'preset'
              ? 'px-2.5 h-9 bg-accent/15 text-accent'
              : 'px-2.5 h-9 hover:bg-accent/5'
          }
        >
          {texts.modePreset}
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={value.mode === 'custom'}
          onClick={() => setMode('custom')}
          className={
            value.mode === 'custom'
              ? 'px-2.5 h-9 bg-accent/15 text-accent'
              : 'px-2.5 h-9 hover:bg-accent/5'
          }
        >
          {texts.modeCustom}
        </button>
      </div>

      {value.mode === 'preset' ? (
        <>
          <span className="text-xs text-muted">{texts.rangeLabel}</span>
          <Select
            value={value.presetKey}
            onChange={(e) =>
              onChange({ mode: 'preset', presetKey: e.target.value })
            }
            className="w-32"
          >
            {presets.map((p) => (
              <option key={p.key} value={p.key}>
                {p.label}
              </option>
            ))}
          </Select>
        </>
      ) : (
        <>
          <span className="text-xs text-muted">{texts.fromLabel}</span>
          <Input
            type="datetime-local"
            value={msToLocalInput(value.fromMs)}
            onChange={(e) => {
              const ms = localInputToMs(e.target.value);
              if (ms !== null) updateCustom({ fromMs: ms });
            }}
            className="w-52"
          />
          <span className="text-xs text-muted">{texts.toLabel}</span>
          <Input
            type="datetime-local"
            value={msToLocalInput(value.toMs)}
            onChange={(e) => {
              const ms = localInputToMs(e.target.value);
              if (ms !== null) updateCustom({ toMs: ms });
            }}
            className="w-52"
          />
          <span className="text-xs text-muted">{texts.stepLabel}</span>
          <Select
            value={value.step}
            onChange={(e) => {
              const next = stepOptions.find((s) => s.value === e.target.value);
              if (next) updateCustom({ step: next.value, stepSecs: next.secs });
            }}
            className="w-24"
          >
            {stepOptions.map((s) => (
              <option key={s.value} value={s.value}>
                {s.label}
              </option>
            ))}
          </Select>
          {customInvalid && (
            <span className="text-xs text-amber-600">{texts.invalidHint}</span>
          )}
        </>
      )}
    </div>
  );
}
