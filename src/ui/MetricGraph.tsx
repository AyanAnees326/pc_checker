import { LiveChart } from "../stress/LiveChart";
import { AnimatedCounter } from "./AnimatedCounter";
import { Stat, useTelemetryFullscreen } from "../ui";

/**
 * A `Stat`-style labeled value with a chart underneath. Renders as a small,
 * non-interactive sparkline inside the compact "Live telemetry" panel, and as a full
 * axis+legend chart when that panel is opened fullscreen (`useTelemetryFullscreen`) —
 * no per-graph click-to-expand. That in-place expand used to render the full chart
 * squeezed into whatever narrow grid cell the sparkline happened to occupy, which
 * produced overlapping axis labels and a visibly broken chart; a single fullscreen
 * toggle for the whole panel (see `App.tsx`) gives every chart real room instead.
 *
 * If there's no real history yet (fewer than two real readings), this renders as a
 * plain `Stat` with no chart — never a broken or empty one. Only genuinely continuous
 * telemetry should be passed here; monotonic counters and OK/FAILED-style values
 * belong in a plain `Stat` instead.
 */
export function MetricGraph({
  label,
  colorClass,
  unit,
  format,
  suffix,
  elapsedMs,
  values,
  throttleBandsMs,
  markersMs,
}: {
  label: string;
  /** Matches a `.series` / `.series-2` / `.series-3` / `.series-4` class. */
  colorClass: string;
  unit?: string;
  format?: (v: number) => string;
  suffix?: string;
  elapsedMs: number[];
  values: (number | null)[];
  throttleBandsMs?: [number, number][];
  markersMs?: { ms: number; label: string }[];
}) {
  const fullscreen = useTelemetryFullscreen();

  let latest: number | null = null;
  let realReadings = 0;
  for (const v of values) {
    if (v !== null && Number.isFinite(v)) {
      latest = v;
      realReadings++;
    }
  }

  if (realReadings < 2) {
    return <Stat label={label} value={latest} suffix={suffix} />;
  }

  const series = [{ label, colorClass, values, unit, format }];

  return (
    <div>
      <span className="block text-muted text-xs uppercase tracking-[0.04em]">{label}</span>
      <span className="block text-[1.35rem] font-semibold tabular-nums">
        {latest === null ? <span className="missing">—</span> : <AnimatedCounter value={latest} format={format} suffix={suffix} />}
      </span>
      <div className={fullscreen ? "mt-[0.6rem]" : "mt-[0.35rem] opacity-90"}>
        {fullscreen ? (
          <LiveChart
            elapsedMs={elapsedMs}
            series={series}
            throttleBandsMs={throttleBandsMs}
            markersMs={markersMs}
            showYTicks
            height={220}
          />
        ) : (
          <LiveChart elapsedMs={elapsedMs} series={series} compact height={40} width={160} />
        )}
      </div>
    </div>
  );
}
