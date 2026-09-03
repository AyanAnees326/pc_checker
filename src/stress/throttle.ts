/** Contiguous [startMs, endMs] ranges where `isThrottling` read true for a sample —
 * shared core for the CPU (single thermal-throttle field) and GPU (multiple
 * throttle-reason fields OR'd together) stress cards, which previously each hand-rolled
 * an identical loop keyed on different fields. */
export function computeThrottleBands<T extends { elapsed_ms: number }>(
  samples: T[],
  isThrottling: (s: T) => boolean
): [number, number][] {
  const bands: [number, number][] = [];
  let start: number | null = null;

  for (const s of samples) {
    if (isThrottling(s)) {
      if (start === null) start = s.elapsed_ms;
    } else if (start !== null) {
      bands.push([start, s.elapsed_ms]);
      start = null;
    }
  }
  if (start !== null && samples.length > 0) {
    bands.push([start, samples[samples.length - 1].elapsed_ms]);
  }
  return bands;
}
