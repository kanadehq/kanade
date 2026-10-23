/** Expire cached current-position labels even while requests fail or the tab sleeps. */
export function locationIsCurrent(
  reading: { current: boolean; acquired_at: string } | null | undefined,
  updatedAt: number,
  now: number,
  refreshFailed: boolean,
): boolean {
  return !!reading?.current && !refreshFailed
    && now - Date.parse(reading.acquired_at) <= 120_000
    && now - updatedAt <= 15_000;
}
