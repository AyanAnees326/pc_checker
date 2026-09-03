/**
 * A live-updating, multi-series time chart for stress-test telemetry.
 *
 * Generalizes the battery history chart's approach (a single deterministic SVG path)
 * to several series at once — clock, power, temperature — each on its own normalized
 * 0-1 scale rather than a shared Y axis, since those quantities live on wildly
 * different ranges and forcing them onto one axis would flatten whichever has the
 * smaller range into a nearly straight line. The legend states each series' actual
 * min/max so the normalization does not hide the real numbers.
 *
 * Gaps (a `null` value, e.g. a sample taken before the telemetry source answered its
 * first reading) break the line rather than interpolating across them or drawing
 * through zero — a missing reading must never look like a real one.
 *
 * `compact`/`width`/`showYTicks`/`markersMs` are additive: every existing call site
 * (the combined CPU/GPU stress-test charts) keeps working unchanged. They exist so
 * `MetricGraph` (a per-metric sparkline that expands into a full chart) can reuse this
 * same normalization/gap-breaking math instead of a second implementation — a compact
 * sparkline drops the axis/legend chrome, and the expanded view can show min/max ticks
 * and labeled markers (e.g. a battery swap) that the always-multi-series combined
 * charts have never needed.
 */

export interface ChartSeries {
  label: string;
  /** Matches a `.series` / `.series-2` / `.series-3` class in styles.css. */
  colorClass: string;
  /** Same length as `elapsedMs`; `null` marks a gap. */
  values: (number | null)[];
  unit?: string;
  /** How to format a value for the legend's min/max, e.g. one decimal place. */
  format?: (v: number) => string;
}

const defaultFormat = (v: number) => (Number.isInteger(v) ? String(v) : v.toFixed(1));

export function LiveChart({
  elapsedMs,
  series,
  throttleBandsMs,
  markersMs,
  height = 180,
  width = 720,
  compact = false,
  showYTicks = false,
}: {
  elapsedMs: number[];
  series: ChartSeries[];
  /** [startMs, endMs] pairs shaded to mark throttling — cause and effect on one axis. */
  throttleBandsMs?: [number, number][];
  /** Labeled dashed vertical lines, e.g. a battery-swap event on a history chart. */
  markersMs?: { ms: number; label: string }[];
  height?: number;
  /** SVG viewBox width. Defaults to the full-size 720 the combined charts use;
   * `MetricGraph`'s sparkline passes something much smaller. */
  width?: number;
  /** Sparkline mode: no axis line, no legend, tighter padding. */
  compact?: boolean;
  /** Render each series' min/max as text next to the plot, using the (otherwise
   * unused outside compact mode) `.tick` class. Most useful with 1-2 series. */
  showYTicks?: boolean;
}) {
  const w = width;
  const pad = compact
    ? { top: 4, right: 4, bottom: 4, left: 4 }
    : { top: 10, right: 12, bottom: 22, left: showYTicks ? 30 : 12 };
  const plotW = w - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;

  const maxMs = Math.max(1, ...elapsedMs, ...(throttleBandsMs ?? []).flat(), ...(markersMs ?? []).map((m) => m.ms));
  const x = (ms: number) => pad.left + (ms / maxMs) * plotW;

  const normalized = series.map((s) => {
    const finite = s.values.filter((v): v is number => v !== null && Number.isFinite(v));
    const min = finite.length ? Math.min(...finite) : 0;
    const max = finite.length ? Math.max(...finite) : 1;
    const range = max - min || 1;
    const y = (v: number) => pad.top + (1 - (v - min) / range) * plotH;
    return { ...s, min, max, y };
  });

  const pathFor = (s: (typeof normalized)[number]) => {
    let d = "";
    let drawing = false;
    for (let i = 0; i < elapsedMs.length; i++) {
      const v = s.values[i];
      if (v === null || !Number.isFinite(v)) {
        drawing = false;
        continue;
      }
      const cmd = drawing ? "L" : "M";
      d += `${cmd} ${x(elapsedMs[i]).toFixed(1)} ${s.y(v).toFixed(1)} `;
      drawing = true;
    }
    return d.trim();
  };

  return (
    <div>
      <svg
        viewBox={`0 0 ${w} ${height}`}
        className={
          "chart w-full h-auto " +
          (compact ? "compact bg-transparent" : "bg-surface-2 border border-border rounded-md")
        }
        role="img"
        aria-label="Live stress telemetry"
      >
        {(throttleBandsMs ?? []).map(([start, end], i) => (
          <rect
            key={i}
            x={x(start)}
            y={pad.top}
            width={Math.max(0, x(end) - x(start))}
            height={plotH}
            className="throttle-band"
          />
        ))}
        {!compact && (
          <line x1={pad.left} y1={height - pad.bottom} x2={w - pad.right} y2={height - pad.bottom} className="axis" />
        )}
        {(markersMs ?? []).map((m, i) => (
          <g key={i}>
            <line x1={x(m.ms)} y1={pad.top} x2={x(m.ms)} y2={height - pad.bottom} className="swap" />
            {!compact && (
              <text x={x(m.ms) + 3} y={pad.top + 10} className="tick">
                {m.label}
              </text>
            )}
          </g>
        ))}
        {normalized.map((s, i) => (
          <path key={i} d={pathFor(s)} className={`series ${s.colorClass}`} />
        ))}
        {showYTicks &&
          normalized.map((s, i) => (
            <g key={i}>
              <text x={2} y={s.y(s.max) + 3} className="tick">
                {(s.format ?? defaultFormat)(s.max)}
              </text>
              <text x={2} y={s.y(s.min) + 3} className="tick">
                {(s.format ?? defaultFormat)(s.min)}
              </text>
            </g>
          ))}
      </svg>
      {!compact && (
        <div className="chart-legend flex gap-4 mt-2 text-[0.78rem] text-muted flex-wrap">
          {normalized.map((s, i) => (
            <span key={i}>
              <span className={`swatch ${s.colorClass}`} style={{ background: `var(--color-${legendVar(s.colorClass)})` }} />
              {s.label}: {(s.format ?? defaultFormat)(s.min)}
              {"–"}
              {(s.format ?? defaultFormat)(s.max)}
              {s.unit ? ` ${s.unit}` : ""}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

/** Maps a chart series CSS class to the CSS variable driving its stroke color, for
 * the inline swatch swab next to the legend text. Dedicated `--chart-*` tokens, not
 * the severity tokens — a telemetry line's color is a plotting choice, not a
 * severity signal. */
function legendVar(colorClass: string): string {
  if (colorClass === "series-2") return "chart-2";
  if (colorClass === "series-3") return "chart-3";
  if (colorClass === "series-4") return "chart-4";
  return "chart-1";
}
