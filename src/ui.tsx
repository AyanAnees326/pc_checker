import { createContext, useCallback, useContext, useEffect, useState } from "react";
import type { ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Finding, Reading, Severity } from "./types";
import { AnimatedCounter } from "./ui/AnimatedCounter";

/** Whether the shared "Live telemetry" panel is currently shown fullscreen — read by
 * `MetricGraph`/`GraphsGroup` so a graph can render as a full axis+legend chart at
 * fullscreen size and a plain sparkline otherwise, without threading a prop through
 * every section that portals graphs into the panel. Portaled content stays inside its
 * calling component's place in the React tree (only its DOM location moves), so
 * context set around `App`'s main content still reaches graphs rendered via a portal
 * from deep inside `CpuSection`/`GpuStressCard`/`BatterySection`. */
export const TelemetryFullscreenContext = createContext(false);
export function useTelemetryFullscreen(): boolean {
  return useContext(TelemetryFullscreenContext);
}

// --- Independent component scanning -----------------------------------------
// Shared by every inventory section and by `CpuSection`: each component is scanned
// independently — there is no combined "scan everything" backend command, so a buyer
// can check just the battery, or just the CPU, without waiting on — or paying the
// cost of — probing the rest of the machine. Each scan fires automatically once on
// mount (see the effect below) rather than waiting for a manual button press, so the
// whole card grid is populated by the time the app finishes opening; `run` is still
// returned so a section can offer a retry after a genuine error.

export type ScanState<T> =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "done"; data: T; findings: Finding[]; scannedAt: string };

interface RawComponentScan<T> {
  data: T;
  findings: Finding[];
  scanned_at: string;
}

export function useComponentScan<T>(command: string) {
  const [state, setState] = useState<ScanState<T>>({ status: "idle" });

  const run = useCallback(async () => {
    setState({ status: "loading" });
    try {
      const result = await invoke<RawComponentScan<T>>(command);
      setState({
        status: "done",
        data: result.data,
        findings: result.findings,
        scannedAt: result.scanned_at,
      });
    } catch (e) {
      setState({ status: "error", message: String(e) });
    }
  }, [command]);

  useEffect(() => {
    run();
  }, [run]);

  return [state, run] as const;
}

// --- Formatting -------------------------------------------------------------

export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / Math.pow(1024, i);
  return `${value >= 100 || i === 0 ? Math.round(value) : value.toFixed(1)} ${units[i]}`;
}

export function formatHours(hours: number): string {
  const years = hours / 8766;
  if (years >= 1) return `${hours.toLocaleString()} h (${years.toFixed(1)} yr)`;
  return `${hours.toLocaleString()} h`;
}

export function formatDuration(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m`;
}

export function formatDate(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

// --- Primitives -------------------------------------------------------------

export function Section({
  title,
  subtitle,
  action,
  statusBadge,
  children,
}: {
  title: string;
  subtitle?: string;
  action?: ReactNode;
  /** A small at-a-glance indicator (e.g. `StatusDot`) shown next to the title. */
  statusBadge?: ReactNode;
  children: ReactNode;
}) {
  // Minimizing only hides this card's own body — any `MetricGraph`s it feeds into the
  // shared telemetry panel live in a portal called from outside this collapse, so
  // minimizing a card never interrupts its live graphs (see each section's own
  // `createPortal` call, kept as a sibling of `<Section>` rather than inside it).
  const [collapsed, setCollapsed] = useState(false);

  return (
    <section className="glass-card rounded-lg p-6">
      <div className="flex items-baseline gap-3 mb-3.5 pb-2.5 border-b border-border flex-wrap">
        <h2 className="m-0 text-[0.95rem] uppercase tracking-[0.06em] text-text">{title}</h2>
        {statusBadge}
        {subtitle && <span className="text-muted text-sm">{subtitle}</span>}
        <div className="ml-auto flex items-center gap-2">
          {action}
          <button
            type="button"
            onClick={() => setCollapsed((c) => !c)}
            aria-expanded={!collapsed}
            aria-label={collapsed ? "Expand card" : "Minimize card"}
            title={collapsed ? "Expand" : "Minimize"}
            className="bg-transparent border-0 cursor-pointer text-muted hover:text-text text-xs leading-none p-1 transition-colors"
          >
            {collapsed ? "▸" : "▾"}
          </button>
        </div>
      </div>
      {!collapsed && children}
    </section>
  );
}

/** Worst (most severe) finding in a list, or `null` if there are none — "worst" in
 * the same order `FindingList` already sorts by (critical shown before ok). */
export function worstSeverity(findings: Finding[]): Severity | null {
  const order: Severity[] = ["critical", "problem", "watch", "ok"];
  for (const s of order) {
    if (findings.some((f) => f.severity === s)) return s;
  }
  return null;
}

/** A small colored/glowing dot summarizing a section's worst finding severity, so a
 * user scanning many cards can spot problem areas without opening each one. */
export function StatusDot({ severity }: { severity: Severity }) {
  return <span className={`status-dot status-dot-${severity}`} title={SEVERITY_LABEL[severity]} />;
}

/** A labeled numeric/text value, e.g. inside a `stat-row`. Shared by every stress
 * card instead of each hand-rolling its own copy. Numeric values tick up/down via
 * `AnimatedCounter`; text values (e.g. "OK"/"FAILED") render as-is — animating a
 * state label would be misleading, only genuine numbers get the tween. */
export function Stat({
  label,
  value,
  suffix,
  format,
}: {
  label: string;
  value: number | string | null;
  suffix?: string;
  format?: (v: number) => string;
}) {
  return (
    <div className="min-w-[90px]">
      <span className="block text-muted text-xs uppercase tracking-[0.04em]">{label}</span>
      <span className="block text-[1.35rem] font-semibold tabular-nums">
        {value === null ? (
          <span className="missing">—</span>
        ) : typeof value === "number" ? (
          <AnimatedCounter value={value} suffix={suffix} format={format} />
        ) : (
          `${value}${suffix ?? ""}`
        )}
      </span>
    </div>
  );
}

/** Shared duration picker for the CPU/GPU stress cards — each passes its own option
 * list and default (the two schedules aren't the same length), but presents the same
 * consistent control. Hidden by call sites while a run is active. */
export function DurationSelect({
  value,
  onChange,
  options,
  defaultMinutes,
}: {
  value: number;
  onChange: (minutes: number) => void;
  options: number[];
  defaultMinutes: number;
}) {
  return (
    <select
      className="duration-select bg-surface-2 text-text border border-border rounded-lg px-2.5 py-1.5 text-[0.82rem]"
      value={value}
      onChange={(e) => onChange(Number(e.target.value))}
      aria-label="Stress test duration"
    >
      {options.map((m) => (
        <option key={m} value={m}>
          {m} min{m === defaultMinutes ? " (default)" : ""}
        </option>
      ))}
    </select>
  );
}

/**
 * Retry action for a component scan. Scans now run automatically on mount (see
 * `useComponentScan`), so there is no manual "scan"/"rescan" trigger for the normal
 * case — this renders nothing unless the scan actually failed, in which case it's the
 * only way to try again.
 */
export function ScanButton({
  status,
  onScan,
  label,
}: {
  status: "idle" | "loading" | "error" | "done";
  onScan: () => void;
  label: string;
}) {
  if (status !== "error") return null;
  return (
    <button
      className="btn-gradient rounded-lg px-3.5 py-1.5 text-[0.82rem] font-semibold whitespace-nowrap border-0 cursor-pointer"
      onClick={onScan}
    >
      Retry {label}
    </button>
  );
}

/**
 * Renders one metric.
 *
 * A missing reading shows the reason it is missing, greyed out. It must never render
 * as a zero or a blank — the whole point of the Reading type is that "we could not
 * measure this" and "this measured zero" are different facts.
 */
export function Field<T>({
  label,
  reading,
  render,
  suffix,
}: {
  label: string;
  reading: Reading<T> | undefined;
  render?: (v: T) => ReactNode;
  suffix?: string;
}) {
  let body: ReactNode;
  if (!reading) {
    body = <span className="missing text-sm">not collected</span>;
  } else if (reading.ok) {
    body = (
      <span className="text-right tabular-nums">
        {render ? render(reading.value) : String(reading.value)}
        {suffix ? ` ${suffix}` : ""}
      </span>
    );
  } else {
    body = <span className="missing text-sm">{reading.note}</span>;
  }

  return (
    <div className="flex justify-between gap-4 py-1.5 border-b border-white/5 text-[0.88rem]">
      <span className="text-muted whitespace-nowrap">{label}</span>
      {body}
    </div>
  );
}

/** A plain label/value pair for data that is always present. */
export function Plain({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex justify-between gap-4 py-1.5 border-b border-white/5 text-[0.88rem]">
      <span className="text-muted whitespace-nowrap">{label}</span>
      <span className="text-right tabular-nums">{children}</span>
    </div>
  );
}

export const SEVERITY_LABEL: Record<Severity, string> = {
  ok: "OK",
  watch: "Watch",
  problem: "Problem",
  critical: "Critical",
};

export function SeverityBadge({ severity }: { severity: Severity }) {
  return (
    <span className={`badge-${severity} inline-block px-2 py-0.5 rounded-full text-[0.7rem] font-bold uppercase tracking-wide`}>
      {SEVERITY_LABEL[severity]}
    </span>
  );
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="text-muted text-sm">{children}</p>;
}

/** Collapsed-by-default dropdown holding a component's full model/spec info — keeps
 * the compact card small while the detail is still one click away. */
export function ComponentDetails({ children, label = "Component details" }: { children: ReactNode; label?: string }) {
  return (
    <details className="group mt-2">
      <summary className="cursor-pointer select-none list-none flex items-center gap-1.5 text-muted text-sm py-1.5">
        <span className="inline-block transition-transform group-open:rotate-90">▸</span>
        {label}
      </summary>
      <div className="mt-2">{children}</div>
    </details>
  );
}

/** A labeled cluster of `MetricGraph`s inside the shared graphs panel — the heading
 * says which component/run it belongs to since several sections can portal into the
 * same panel at once (e.g. CPU stress and GPU stress running together). Columns widen
 * in fullscreen mode since `MetricGraph` itself renders a full axis+legend chart there
 * instead of a compact sparkline. */
export function GraphsGroup({ title, children }: { title: string; children: ReactNode }) {
  const fullscreen = useTelemetryFullscreen();
  return (
    <div>
      <h4 className="m-0 mb-2 text-[0.8rem] uppercase tracking-[0.05em] text-muted">{title}</h4>
      <div
        className={
          "grid gap-x-6 gap-y-4 " +
          (fullscreen ? "grid-cols-[repeat(auto-fit,minmax(280px,1fr))]" : "grid-cols-[repeat(auto-fit,minmax(160px,1fr))]")
        }
      >
        {children}
      </div>
    </div>
  );
}

/** Shimmer placeholder shown in place of a card's body while a scan is in flight —
 * a multi-second SMART/ACPI read otherwise leaves the card looking frozen with
 * nothing below the "Scanning…" button label. */
export function Skeleton() {
  return (
    <div>
      <div className="skeleton skeleton-line h-[0.9rem] my-2" />
      <div className="skeleton skeleton-line h-[0.9rem] my-2" />
      <div className="skeleton skeleton-line h-[0.9rem] my-2" />
      <div className="skeleton skeleton-line h-[0.9rem] my-2" />
    </div>
  );
}

// --- Findings -----------------------------------------------------------------
// Shared between every scanned component and the stress-test cards: whatever
// produced a `Finding[]`, it renders the same way — worst-first, with the passing
// checks folded away rather than competing for attention with the ones that matter.

export function FindingList({ findings }: { findings: Finding[] }) {
  const defects = findings.filter((f) => f.severity !== "ok");
  const ok = findings.filter((f) => f.severity === "ok");

  if (defects.length === 0 && ok.length === 0) {
    return <p className="text-muted text-sm">No findings were produced from this scan.</p>;
  }

  return (
    <>
      {defects.length === 0 && <p className="text-muted text-sm">Nothing of concern in this check.</p>}
      {defects.map((f) => (
        <FindingCard key={f.id} finding={f} />
      ))}
      {ok.length > 0 && (
        <details className="mt-2">
          <summary className="cursor-pointer text-muted text-sm py-1.5">
            {ok.length} check{ok.length === 1 ? "" : "s"} passed
          </summary>
          {ok.map((f) => (
            <FindingCard key={f.id} finding={f} />
          ))}
        </details>
      )}
    </>
  );
}

export function FindingCard({ finding }: { finding: Finding }) {
  return (
    <article className={`glass-panel finding-${finding.severity} border-l-[3px] rounded-lg p-4 mb-3`}>
      <div className="flex items-center gap-2.5 mb-2 flex-wrap">
        <SeverityBadge severity={finding.severity as Severity} />
        <h3 className="m-0 text-[0.98rem] font-semibold">{finding.title}</h3>
        {finding.estimated_cost_usd !== undefined && (
          <span className="ml-auto text-watch font-semibold">~${finding.estimated_cost_usd}</span>
        )}
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 mb-2.5 text-[0.85rem]">
        <dt className="text-muted">Observed</dt>
        <dd className="m-0">{finding.observed}</dd>
        <dt className="text-muted">Expected</dt>
        <dd className="m-0">{finding.expected}</dd>
        <dt className="text-muted">Basis</dt>
        <dd className="m-0 text-muted">{finding.basis}</dd>
      </dl>
      <p className="m-0 text-[0.88rem] text-[#cfd5e2]">{finding.recommendation}</p>
    </article>
  );
}
