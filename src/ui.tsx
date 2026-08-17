import { useCallback, useState } from "react";
import type { ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Finding, Reading, Severity } from "./types";

// --- Independent component scanning -----------------------------------------
// Shared by every inventory section and by `CpuSection`: each component is scanned
// independently and on demand — there is no combined "scan everything" backend
// command, so a buyer can check just the battery, or just the CPU, without waiting
// on — or paying the cost of — probing the rest of the machine.

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
  children,
}: {
  title: string;
  subtitle?: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="section">
      <div className="section-head">
        <h2>{title}</h2>
        {subtitle && <span className="section-sub">{subtitle}</span>}
        {action && <div className="section-action">{action}</div>}
      </div>
      {children}
    </section>
  );
}

/**
 * The scan trigger every component section uses. Each component is scanned
 * independently and on demand — there is no combined "scan everything" backend
 * command, so this button is always wired to exactly one Tauri command.
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
  return (
    <button className="scan-btn" onClick={onScan} disabled={status === "loading"}>
      {status === "loading" ? "Scanning…" : status === "done" ? `Rescan ${label}` : `Scan ${label}`}
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
    body = <span className="missing">not collected</span>;
  } else if (reading.ok) {
    body = (
      <span className="value">
        {render ? render(reading.value) : String(reading.value)}
        {suffix ? ` ${suffix}` : ""}
      </span>
    );
  } else {
    body = <span className="missing">{reading.note}</span>;
  }

  return (
    <div className="field">
      <span className="label">{label}</span>
      {body}
    </div>
  );
}

/** A plain label/value pair for data that is always present. */
export function Plain({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="field">
      <span className="label">{label}</span>
      <span className="value">{children}</span>
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
  return <span className={`badge badge-${severity}`}>{SEVERITY_LABEL[severity]}</span>;
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="empty">{children}</p>;
}

// --- Findings -----------------------------------------------------------------
// Shared between every scanned component and the stress-test cards: whatever
// produced a `Finding[]`, it renders the same way — worst-first, with the passing
// checks folded away rather than competing for attention with the ones that matter.

export function FindingList({ findings }: { findings: Finding[] }) {
  const defects = findings.filter((f) => f.severity !== "ok");
  const ok = findings.filter((f) => f.severity === "ok");

  if (defects.length === 0 && ok.length === 0) {
    return <p className="empty">No findings were produced from this scan.</p>;
  }

  return (
    <>
      {defects.length === 0 && <p className="empty">Nothing of concern in this check.</p>}
      {defects.map((f) => (
        <FindingCard key={f.id} finding={f} />
      ))}
      {ok.length > 0 && (
        <details className="ok-details">
          <summary>
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
    <article className={`finding finding-${finding.severity}`}>
      <div className="finding-head">
        <SeverityBadge severity={finding.severity as Severity} />
        <h3>{finding.title}</h3>
        {finding.estimated_cost_usd !== undefined && <span className="cost">~${finding.estimated_cost_usd}</span>}
      </div>
      <dl>
        <dt>Observed</dt>
        <dd>{finding.observed}</dd>
        <dt>Expected</dt>
        <dd>{finding.expected}</dd>
        <dt>Basis</dt>
        <dd className="muted">{finding.basis}</dd>
      </dl>
      <p className="advice">{finding.recommendation}</p>
    </article>
  );
}
