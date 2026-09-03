import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { createPortal } from "react-dom";

import type {
  CpuLiveSample,
  CpuSample,
  CpuStressResult,
  CpuTopology,
  Finding,
  StressPhase,
  StressStartedEvent,
} from "../types";
import { PHASE_LABEL, STRESS_PHASES, readingValue } from "../types";
import {
  ComponentDetails,
  DurationSelect,
  Empty,
  Field,
  FindingList,
  GraphsGroup,
  Plain,
  ScanButton,
  Section,
  Skeleton,
  Stat,
  StatusDot,
  useComponentScan,
  worstSeverity,
} from "../ui";
import { MetricGraph } from "../ui/MetricGraph";
import { computeThrottleBands } from "../stress/throttle";

/**
 * The CPU card: a compact status/controls surface (topology scan, live monitor,
 * stress test) with the full topology spec tucked behind a `ComponentDetails`
 * dropdown, and every `MetricGraph` teleported into the shared graphs panel below
 * the card grid via `graphsContainer` rather than rendered inline — the card stays
 * small regardless of how much telemetry is streaming.
 */

// While a stress run is active, the chart is fed only the trailing window below
// instead of the full growing sample array. `LiveChart.pathFor` rebuilds each
// series' SVG path from scratch on every render with no memoization — feeding it an
// unbounded array that grows for up to ~12 minutes at 4 Hz means per-tick render cost
// grows for the whole run (O(n^2) total). A bounded window keeps render cost per tick
// constant regardless of elapsed time; the full result is still shown once, in full,
// on the terminal "done" render, where a single O(n) cost is fine.
const STRESS_CHART_WINDOW = 240; // ~60s at 4 Hz

// The live monitor's own rolling buffer, capped the same way so it can be left
// running indefinitely without unbounded growth.
const MONITOR_WINDOW = 120; // ~2 minutes at 1 Hz

function pushCapped<T>(arr: T[], item: T, cap: number): T[] {
  const base = arr.length >= cap ? arr.slice(arr.length - cap + 1) : arr;
  return [...base, item];
}

/** Rebase a window of samples so its first point sits at elapsed_ms = 0 — otherwise
 * a trailing slice of a long run would render scrunched against the chart's right
 * edge instead of as a proper sliding window. */
function rebaseElapsed<T extends { elapsed_ms: number }>(samples: T[]): T[] {
  if (samples.length === 0) return samples;
  const base = samples[0].elapsed_ms;
  return base === 0 ? samples : samples.map((s) => ({ ...s, elapsed_ms: s.elapsed_ms - base }));
}

type StressState =
  | { status: "idle" }
  | { status: "running"; samples: CpuSample[] }
  | { status: "done"; result: CpuStressResult; findings: Finding[] }
  | { status: "error"; message: string };

interface CompletePayload {
  data: CpuStressResult;
  findings: Finding[];
  scanned_at: string;
}

type MonitorState =
  | { status: "stopped" }
  | { status: "running"; samples: CpuLiveSample[] }
  | { status: "error"; message: string };

const buttonClass =
  "btn-gradient rounded-lg px-3.5 py-1.5 text-[0.82rem] font-semibold whitespace-nowrap border-0 cursor-pointer";
const errorClass = "text-problem bg-problem/12 border border-problem/30 rounded-lg px-4 py-3";

export function CpuSection({ graphsContainer }: { graphsContainer: HTMLDivElement | null }) {
  const [topology, runTopologyScan] = useComponentScan<CpuTopology>("scan_cpu");
  const [pawnioStatus, setPawnioStatus] = useState<
    { ready: true } | { ready: false; detail: string } | null
  >(null);

  const [monitor, setMonitor] = useState<MonitorState>({ status: "stopped" });
  const monitorUnlisten = useRef<(() => void) | null>(null);

  const [stress, setStress] = useState<StressState>({ status: "idle" });
  const stressUnlistenRefs = useRef<Array<() => void>>([]);
  const [durationMinutes, setDurationMinutes] = useState(14);

  useEffect(() => {
    invoke<{ state: string; detail?: string }>("pawnio_status")
      .then((s) =>
        setPawnioStatus(
          s.state === "installed" ? { ready: true } : { ready: false, detail: s.detail ?? "unknown reason" }
        )
      )
      .catch(() => setPawnioStatus(null));

    return () => {
      monitorUnlisten.current?.();
      stressUnlistenRefs.current.forEach((u) => u());
    };
  }, []);

  const startMonitor = useCallback(async () => {
    setMonitor({ status: "running", samples: [] });

    // `listen` is inside the try, not before it: subscribing is itself a fallible
    // IPC call (Tauri gates the event system behind a capability), and leaving it
    // unguarded meant a rejection left the UI showing "running" forever with no
    // samples and no error — which is exactly how a missing capability hid itself.
    try {
      const unlisten = await listen<CpuLiveSample>("telemetry://cpu/live", (e) => {
        setMonitor((prev) =>
          prev.status === "running"
            ? { ...prev, samples: pushCapped(prev.samples, e.payload, MONITOR_WINDOW) }
            : prev
        );
      });
      monitorUnlisten.current = unlisten;

      await invoke("start_cpu_monitor");
    } catch (e) {
      setMonitor({ status: "error", message: String(e) });
      monitorUnlisten.current?.();
      monitorUnlisten.current = null;
    }
  }, []);

  const stopMonitor = useCallback(() => {
    invoke("stop_cpu_monitor").catch(() => {});
    monitorUnlisten.current?.();
    monitorUnlisten.current = null;
    setMonitor({ status: "stopped" });
  }, []);

  const startStress = useCallback(async () => {
    // Mutually exclusive with the live monitor on the backend (both would otherwise
    // want their own PawnIO MSR session) — stop it here too so the UI does not keep
    // showing a monitor that the backend already tore down.
    if (monitor.status === "running") {
      stopMonitor();
    }

    setStress({ status: "running", samples: [] });

    // Subscribing is fallible too — see `startMonitor` for why this must be guarded.
    try {
      const unlistenStarted = await listen<StressStartedEvent>("stress://cpu/started", () => {});

      const unlistenSample = await listen<CpuSample>("stress://cpu/sample", (e) => {
        setStress((prev) =>
          prev.status === "running" ? { ...prev, samples: [...prev.samples, e.payload] } : prev
        );
      });

      const unlistenComplete = await listen<CompletePayload>("stress://cpu/complete", (e) => {
        setStress({ status: "done", result: e.payload.data, findings: e.payload.findings });
        stressUnlistenRefs.current.forEach((u) => u());
        stressUnlistenRefs.current = [];
      });

      stressUnlistenRefs.current = [unlistenStarted, unlistenSample, unlistenComplete];

      await invoke("start_cpu_stress", { durationSecs: durationMinutes * 60 });
    } catch (e) {
      setStress({ status: "error", message: String(e) });
      stressUnlistenRefs.current.forEach((u) => u());
      stressUnlistenRefs.current = [];
    }
  }, [monitor.status, stopMonitor, durationMinutes]);

  const cancelStress = useCallback(() => {
    invoke("cancel_cpu_stress").catch(() => {});
  }, []);

  const stressSamples = stress.status === "running" ? stress.samples : stress.status === "done" ? stress.result.samples : [];
  const latestStress = stressSamples[stressSamples.length - 1];
  const currentPhase: StressPhase | null = latestStress?.phase ?? null;
  const currentPhaseIndex = currentPhase ? STRESS_PHASES.indexOf(currentPhase) : -1;

  const chartSamples =
    stress.status === "running"
      ? rebaseElapsed(stress.samples.slice(-STRESS_CHART_WINDOW))
      : stress.status === "done"
      ? stress.result.samples
      : [];
  const chartElapsedMs = chartSamples.map((s) => s.elapsed_ms);
  const chartThrottleBands = computeThrottleBands(chartSamples, (s) => readingValue(s.thermal_throttling) === true);

  const monitorChartSamples = monitor.status === "running" ? rebaseElapsed(monitor.samples) : [];
  const monitorElapsedMs = monitorChartSamples.map((s) => s.elapsed_ms);

  const severity = worstSeverity([
    ...(topology.status === "done" ? topology.findings : []),
    ...(stress.status === "done" ? stress.findings : []),
  ]);

  const cpuName = topology.status === "done" ? readingValue(topology.data.brand_string) : null;

  return (
    <>
      <Section
        title="CPU"
        subtitle={cpuName ?? "Live metrics and the full stress test"}
        action={<ScanButton status={topology.status} onScan={runTopologyScan} label="topology" />}
        statusBadge={severity && <StatusDot severity={severity} />}
      >
      {topology.status === "idle" && (
        <Empty>Reads vendor, brand string, core counts and base clock via CPUID. No load is applied.</Empty>
      )}
      {topology.status === "loading" && <Skeleton />}
      {topology.status === "error" && <p className={errorClass}>{topology.message}</p>}
      {topology.status === "done" && (
        <ComponentDetails>
          <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-x-6 gap-y-[0.15rem] mb-3">
            <Field label="Model" reading={topology.data.brand_string} />
            <Field label="Physical cores" reading={topology.data.physical_cores} />
            <Plain label="Logical processors">{topology.data.logical_processors}</Plain>
            <Field label="Base clock" reading={topology.data.base_clock_mhz} suffix="MHz" />
          </div>
          <p className="text-muted text-sm">
            Reading a CPU's internal power/thermal registers isn't possible from ordinary user-space
            code on Windows — it genuinely requires a kernel driver. Tools like MSI Afterburner
            aren't an exception to this: Afterburner silently installs its own driver
            (<code>RTCore64.sys</code>) as part of its own installer, and that driver grants{" "}
            <em>any</em> program unrestricted read/write access to memory and CPU registers — which
            is exactly why it's flagged in security tooling as an abusable driver, the same category
            WinRing0 (the older driver most hardware monitors used) was blocklisted for. PawnIO takes
            the opposite approach: it only runs small, specific, auditable modules rather than
            exposing raw access to whatever asks. Installing it is optional here, and the stress test
            and its correctness check run identically with or without it — only the power/thermal
            telemetry depends on it.
          </p>
        </ComponentDetails>
      )}

      {pawnioStatus !== null && !pawnioStatus.ready && (
        <p className="mt-3 italic text-muted text-sm">
          PawnIO clock, power and temperature readings are unavailable ({pawnioStatus.detail}) — the
          self-check (which catches real computation faults) still runs normally regardless. If PawnIO
          isn't installed, get it from{" "}
          <a href="https://pawnio.eu/" target="_blank" rel="noreferrer" className="text-accent-2">
            pawnio.eu
          </a>
          .
        </p>
      )}

      <div className="mt-3 pt-3 border-t border-border">
        <div className="flex items-center gap-2 flex-wrap">
          {monitor.status === "running" ? (
            <button className={buttonClass} onClick={stopMonitor}>
              Stop live metrics
            </button>
          ) : (
            <button className={buttonClass} onClick={startMonitor} disabled={stress.status === "running"}>
              Start live metrics
            </button>
          )}
          {stress.status !== "running" && (
            <DurationSelect
              value={durationMinutes}
              onChange={setDurationMinutes}
              options={[3, 5, 10, 14, 20, 30]}
              defaultMinutes={14}
            />
          )}
          {stress.status === "running" ? (
            <button className={buttonClass} onClick={cancelStress}>
              Cancel stress test
            </button>
          ) : (
            <button className={buttonClass} onClick={startStress}>
              {stress.status === "done" ? "Run stress test again" : "Start stress test"}
            </button>
          )}
        </div>

        {monitor.status === "stopped" && stress.status === "idle" && (
          <Empty>
            Live metrics show current clock/power/temperature; the stress test adds sustained
            all-core load with the same telemetry, plus cache/memory traffic.
          </Empty>
        )}
        {monitor.status === "error" && <p className={`${errorClass} mt-3`}>{monitor.message}</p>}
        {stress.status === "error" && <p className={`${errorClass} mt-3`}>{stress.message}</p>}

        {(stress.status === "running" || stress.status === "done") && (
          <div className="flex gap-1.5 mt-3 flex-wrap">
            {STRESS_PHASES.map((phase, i) => (
              <span
                key={phase}
                className={
                  "phase-chip" +
                  (i === currentPhaseIndex ? " active" : i < currentPhaseIndex ? " done" : "")
                }
              >
                {PHASE_LABEL[phase]}
              </span>
            ))}
          </div>
        )}

        {latestStress && (stress.status === "running" || stress.status === "done") && (
          <div className="flex gap-6 flex-wrap mt-3">
            <Stat
              label="Self-check"
              value={latestStress.self_check_ok === null ? null : latestStress.self_check_ok ? "OK" : "FAILED"}
            />
            <Stat
              label="Iterations"
              value={latestStress.total_iterations}
              format={(v) => Math.round(v).toLocaleString()}
            />
          </div>
        )}

        {stress.status === "done" && stress.result.aborted && (
          <p className="mt-3 italic text-muted text-sm">
            Run stopped early{stress.result.abort_reason ? `: ${stress.result.abort_reason}` : "."}
          </p>
        )}

        {stress.status === "done" && (
          <div className="mt-4">
            <FindingList findings={stress.findings} />
          </div>
        )}
      </div>
      </Section>

      {graphsContainer &&
        createPortal(
          <>
            {monitor.status === "running" && (
              <GraphsGroup title="CPU — Live metrics">
                <MetricGraph
                  label="Clock"
                  colorClass="series"
                  unit="MHz"
                  suffix="MHz"
                  elapsedMs={monitorElapsedMs}
                  values={monitorChartSamples.map((s) => readingValue(s.effective_clock_mhz))}
                />
                <MetricGraph
                  label="Power"
                  colorClass="series-2"
                  unit="W"
                  suffix="W"
                  elapsedMs={monitorElapsedMs}
                  values={monitorChartSamples.map((s) => readingValue(s.package_power_watts))}
                />
                <MetricGraph
                  label="Temp"
                  colorClass="series-3"
                  unit="°C"
                  suffix="°C"
                  elapsedMs={monitorElapsedMs}
                  values={monitorChartSamples.map((s) => readingValue(s.package_temperature_c))}
                />
              </GraphsGroup>
            )}
            {(stress.status === "running" || stress.status === "done") && latestStress && (
              <GraphsGroup title="CPU — Stress test">
                <MetricGraph
                  label="Clock"
                  colorClass="series"
                  unit="MHz"
                  suffix="MHz"
                  elapsedMs={chartElapsedMs}
                  values={chartSamples.map((s) => readingValue(s.effective_clock_mhz))}
                  throttleBandsMs={chartThrottleBands}
                />
                <MetricGraph
                  label="Power"
                  colorClass="series-2"
                  unit="W"
                  suffix="W"
                  elapsedMs={chartElapsedMs}
                  values={chartSamples.map((s) => readingValue(s.package_power_watts))}
                  throttleBandsMs={chartThrottleBands}
                />
                <MetricGraph
                  label="Temp"
                  colorClass="series-3"
                  unit="°C"
                  suffix="°C"
                  elapsedMs={chartElapsedMs}
                  values={chartSamples.map((s) => readingValue(s.package_temperature_c))}
                  throttleBandsMs={chartThrottleBands}
                />
              </GraphsGroup>
            )}
          </>,
          graphsContainer
        )}
    </>
  );
}
