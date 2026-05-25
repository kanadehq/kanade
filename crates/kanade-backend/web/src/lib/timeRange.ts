import { useEffect, useMemo, useState } from 'react';

/** A "Last N" choice surfaced by the picker as a one-click preset.
 *  `step` / `stepSecs` are the server-side bucket size paired with
 *  that window — picked so each preset lands in ~30–200 Recharts
 *  points without over-fetching. */
export interface PresetOption {
  key: string;
  /** Pre-translated label shown in the Select. */
  label: string;
  fromSecondsAgo: number;
  step: string;
  stepSecs: number;
}

/** Custom-mode bucket sizes. The `value` is sent verbatim to the
 *  server's `step` query param (parsed by humantime), `secs` is the
 *  matching seconds count used to floor "now" to a bucket boundary. */
export interface StepOption {
  value: string;
  secs: number;
  /** Pre-translated label. */
  label: string;
}

/** The bucket sizes both chart pages currently surface in custom
 *  mode. Pages still construct their own `StepOption[]` so they
 *  can scope the i18n key per chart, but the `value → secs`
 *  mapping is shared — the previous per-page copies trivially
 *  drifted (Gemini #229 medium). */
export const DEFAULT_STEP_KEYS = ['30s', '1m', '5m', '15m', '1h', '4h'] as const;
export type DefaultStepKey = (typeof DEFAULT_STEP_KEYS)[number];
export const DEFAULT_STEP_SECS: Record<DefaultStepKey, number> = {
  '30s': 30,
  '1m': 60,
  '5m': 300,
  '15m': 900,
  '1h': 3600,
  '4h': 14400,
};

export type RangeValue =
  | { mode: 'preset'; presetKey: string }
  | { mode: 'custom'; fromMs: number; toMs: number; step: string; stepSecs: number };

export interface ResolvedRange {
  fromIso: string;
  toIso: string;
  step: string;
  stepSecs: number;
  /** True when the resolved range is unusable — caller should
   *  `enabled: false` the React Query and show a hint instead of
   *  firing an inevitable 400. Custom mode with from >= to or
   *  missing fields is the only producer today; preset always
   *  resolves valid. */
  isInvalid: boolean;
}

/** Pure resolver — given a value + presets + a "now" anchor, returns
 *  the {from, to, step} the query should use. Extracted so it can be
 *  called both from the React hook (live ticking) and from tests
 *  without a fake timer. */
export function resolveRange(
  value: RangeValue,
  presets: readonly PresetOption[],
  nowMs: number,
): ResolvedRange {
  if (value.mode === 'preset') {
    // Fall back to the first preset if the saved key disappeared —
    // shouldn't happen in practice, but cheaper than a runtime crash.
    const preset =
      presets.find((p) => p.key === value.presetKey) ?? presets[0];
    if (!preset) {
      return {
        fromIso: new Date(nowMs).toISOString(),
        toIso: new Date(nowMs).toISOString(),
        step: '1m',
        stepSecs: 60,
        isInvalid: true,
      };
    }
    const stepMs = preset.stepSecs * 1000;
    const toMs = Math.floor(nowMs / stepMs) * stepMs;
    const fromMs = toMs - preset.fromSecondsAgo * 1000;
    return {
      fromIso: new Date(fromMs).toISOString(),
      toIso: new Date(toMs).toISOString(),
      step: preset.step,
      stepSecs: preset.stepSecs,
      isInvalid: false,
    };
  }
  // custom
  const fromFinite = Number.isFinite(value.fromMs);
  const toFinite = Number.isFinite(value.toMs);
  const invalid = !fromFinite || !toFinite || value.fromMs >= value.toMs;
  // Even when `invalid`, we still have to return *some* ISO string —
  // callers gate the React Query off `isInvalid`, but the render path
  // for input controls reads back `fromIso`/`toIso` regardless, and
  // `new Date(NaN).toISOString()` throws RangeError. Fall back to
  // `nowMs` so the component keeps rendering and the error message
  // surfaces via the hint text instead of an unhandled exception.
  return {
    fromIso: new Date(fromFinite ? value.fromMs : nowMs).toISOString(),
    toIso: new Date(toFinite ? value.toMs : nowMs).toISOString(),
    step: value.step,
    stepSecs: value.stepSecs,
    isInvalid: invalid,
  };
}

/** Drives a once-per-`tickMs` re-render so preset ranges advance
 *  their right edge with the wall clock. Custom ranges don't move —
 *  the hook still ticks (the cost is one setState/sec at most) but
 *  the resolver returns a fixed window, so React Query keys stay put
 *  and no refetch fires. */
export function useResolvedRange(
  value: RangeValue,
  presets: readonly PresetOption[],
  tickMs = 30_000,
): ResolvedRange {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    if (value.mode === 'custom') return; // freeze the clock
    const id = setInterval(() => setNowMs(Date.now()), tickMs);
    return () => clearInterval(id);
  }, [value.mode, tickMs]);
  return useMemo(
    () => resolveRange(value, presets, nowMs),
    [value, presets, nowMs],
  );
}

/** `<input type="datetime-local">` round-trip helpers. The control
 *  speaks "local wall-clock with no zone" (`YYYY-MM-DDTHH:mm`); we
 *  store everything as epoch ms so the resolver stays zone-agnostic. */
export function msToLocalInput(ms: number): string {
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return '';
  const pad = (n: number) => String(n).padStart(2, '0');
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `T${pad(d.getHours())}:${pad(d.getMinutes())}`
  );
}

export function localInputToMs(s: string): number | null {
  if (!s) return null;
  // `new Date("YYYY-MM-DDTHH:mm")` is parsed as local time by every
  // major engine — exactly what the control hands us back.
  const ms = new Date(s).getTime();
  return Number.isNaN(ms) ? null : ms;
}
