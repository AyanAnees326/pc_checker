import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

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
import { Empty, Field, FindingList, Plain, ScanButton, Section, useComponentScan } from "../ui";
import { LiveChart } from "../stress/LiveChart";

/**
 * The CPU section: topology, a standalone live metrics monitor (clock/wattage/temp,
 * reviewable without running the stress test), and the stress test itself — one
 * section per component, per component owning everything about it, rather than
 * topology and stress living in separate cards the way they used to.
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

export function CpuSection() {
  const [topology, runTopologyScan] = useComponentScan<CpuTopology>("scan_cpu");
  const [pawnioStatus, setPawnioStatus] = useState<
    { ready: true } | { ready: false; detail: string } | null
  >(null);

  const [monitor, setMonitor] = useState<MonitorState>({ status: "stopped" });
  const monitorUnlisten = useRef<(() => void) | null>(null);

  const [stress, setStress] = useState<StressState>({ status: "idle" });
  const stressUnlistenRefs = useRef<Array<() => void>>([]);

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

      await invoke("start_cpu_stress");
    } catch (e) {
      setStress({ status: "error", message: String(e) });
      stressUnlistenRefs.current.forEach((u) => u());
      stressUnlistenRefs.current = [];
    }
  }, [monitor.status, stopMonitor]);

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

  const latestLive = monitor.status === "running" ? monitor.samples[monitor.samples.length - 1] : undefined;
  const monitorChartSamples = monitor.status === "running" ? rebaseElapsed(monitor.samples) : [];

  return (
    <Section
      title="CPU"
      subtitle="Topology, live metrics, and the full stress test — everything about this part in one place"
      action={<ScanButton status={topology.status} onScan={runTopologyScan} label="topology" />}
    >
      {topology.status === "idle" && (
        <Empty>Reads vendor, brand string, core counts and base clock via CPUID. No load is applied.</Empty>
      )}
      {topology.status === "error" && <p className="error">{topology.message}</p>}
      {topology.status === "done" && (
        <div className="grid" style={{ marginBottom: "1rem" }}>
          <Field label="Model" reading={topology.data.brand_string} />
          <Field label="Physical cores" reading={topology.data.physical_cores} />
          <Plain label="Logical processors">{topology.data.logical_processors}</Plain>
          <Field label="Base clock" reading={topology.data.base_clock_mhz} suffix="MHz" />
        </div>
      )}

      {pawnioStatus !== null && !pawnioStatus.ready && (
        <p className="note">
          PawnIO clock, power and temperature readings are unavailable ({pawnioStatus.detail}) — the
          self-check (which catches real computation faults) still runs normally regardless. If PawnIO
          isn't installed, get it from{" "}
          <a href="https://pawnio.eu/" target="_blank" rel="noreferrer">
            pawnio.eu
          </a>
          .
        </p>
      )}

      <details className="ok-details">
        <summary>Why does full diagnostics need a driver at all?</summary>
        <p className="muted" style={{ marginTop: "0.5rem" }}>
          Reading a CPU's internal power/thermal registers isn't possible from ordinary
          user-space code on Windows — it genuinely requires a kernel driver. Tools like MSI
          Afterburner aren't an exception to this: Afterburner silently installs its own driver
          (<code>RTCore64.sys</code>) as part of its own installer, and that driver grants{" "}
          <em>any</em> program unrestricted read/write access to memory and CPU registers — which
          is exactly why it's flagged in security tooling as an abusable driver, the same category
          WinRing0 (the older driver most hardware monitors used) was blocklisted for. PawnIO takes
          the opposite approach: it only runs small, specific, auditable modules rather than
          exposing raw access to whatever asks. Installing it is optional here, and the stress test
          and its correctness check run identically with or without it — only the power/thermal
          telemetry depends on it.
        </p>
      </details>

      <div className="subsection">
        <div className="section-head">
          <h3>Live metrics</h3>
          <div className="section-action">
            {monitor.status === "running" ? (
              <button className="scan-btn" onClick={stopMonitor}>
                Stop live metrics
              </button>
            ) : (
              <button className="scan-btn" onClick={startMonitor} disabled={stress.status === "running"}>
                Start live metrics
              </button>
            )}
          </div>
        </div>

        {monitor.status === "stopped" && (
          <Empty>
            Clock speed, wattage and temperature, updated about once a second — no need to run the
            full stress test just to see current CPU metrics.
          </Empty>
        )}
        {monitor.status === "error" && <p className="error">{monitor.message}</p>}
        {monitor.status === "running" && (
          <>
            {latestLive && (
              <div className="stat-row">
                <Stat label="Clock" value={readingValue(latestLive.effective_clock_mhz)} suffix=" MHz" />
                <Stat label="Power" value={readingValue(latestLive.package_power_watts)} suffix=" W" />
                <Stat label="Temp" value={readingValue(latestLive.package_temperature_c)} suffix=" °C" />
              </div>
            )}
            {monitorChartSamples.length > 1 && (
              <LiveChart
                elapsedMs={monitorChartSamples.map((s) => s.elapsed_ms)}
                series={[
                  {
                    label: "Clock",
                    colorClass: "series",
                    unit: "MHz",
                    values: monitorChartSamples.map((s) => readingValue(s.effective_clock_mhz)),
                  },
                  {
                    label: "Power",
                    colorClass: "series-2",
                    unit: "W",
                    values: monitorChartSamples.map((s) => readingValue(s.package_power_watts)),
                  },
                  {
                    label: "Temp",
                    colorClass: "series-3",
                    unit: "°C",
                    values: monitorChartSamples.map((s) => readingValue(s.package_temperature_c)),
                  },
                ]}
              />
            )}
          </>
        )}
      </div>

      <div className="subsection">
        <div className="section-head">
          <h3>Stress test</h3>
          <span className="section-sub">~12 minutes — sustained load with the same telemetry</span>
          <div className="section-action">
            {stress.status === "running" ? (
              <button className="scan-btn" onClick={cancelStress}>
                Cancel
              </button>
            ) : (
              <button className="scan-btn" onClick={startStress}>
                {stress.status === "done" ? "Run again" : "Start CPU stress test"}
              </button>
            )}
          </div>
        </div>

        {stress.status === "idle" && (
          <Empty>
            Runs real FMA workloads on every core, self-checking every result for computation errors,
            while recording clock speed, power draw and temperature (when PawnIO is available).
          </Empty>
        )}

        {stress.status === "error" && <p className="error">{stress.message}</p>}

        {(stress.status === "running" || stress.status === "done") && (
          <>
            <div className="phase-strip">
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

            {latestStress && (
              <div className="stat-row">
                <Stat label="Clock" value={readingValue(latestStress.effective_clock_mhz)} suffix=" MHz" />
                <Stat label="Power" value={readingValue(latestStress.package_power_watts)} suffix=" W" />
                <Stat label="Temp" value={readingValue(latestStress.package_temperature_c)} suffix=" °C" />
                <Stat
                  label="Self-check"
                  value={latestStress.self_check_ok === null ? null : latestStress.self_check_ok ? "OK" : "FAILED"}
                />
                <Stat label="Iterations" value={latestStress.total_iterations.toLocaleString()} />
              </div>
            )}

            {chartSamples.length > 1 && (
              <LiveChart
                elapsedMs={chartSamples.map((s) => s.elapsed_ms)}
                throttleBandsMs={throttleBands(chartSamples)}
                series={[
                  {
                    label: "Clock",
                    colorClass: "series",
                    unit: "MHz",
                    values: chartSamples.map((s) => readingValue(s.effective_clock_mhz)),
                  },
                  {
                    label: "Power",
                    colorClass: "series-2",
                    unit: "W",
                    values: chartSamples.map((s) => readingValue(s.package_power_watts)),
                  },
                  {
                    label: "Temp",
                    colorClass: "series-3",
                    unit: "°C",
                    values: chartSamples.map((s) => readingValue(s.package_temperature_c)),
                  },
                ]}
              />
            )}

            {stress.status === "done" && (
              <>
                {stress.result.aborted && (
                  <p className="note">
                    Run stopped early{stress.result.abort_reason ? `: ${stress.result.abort_reason}` : "."}
                  </p>
                )}
                <div style={{ marginTop: "1rem" }}>
                  <FindingList findings={stress.findings} />
                </div>
              </>
            )}
          </>
        )}
      </div>
    </Section>
  );
}

function Stat({ label, value, suffix }: { label: string; value: number | string | null; suffix?: string }) {
  return (
    <div className="stat">
      <span className="stat-label">{label}</span>
      <span className="stat-value">
        {value === null ? <span className="missing">—</span> : `${value}${suffix ?? ""}`}
      </span>
    </div>
  );
}

/** Contiguous [startMs, endMs] ranges where the thermal-throttle bit read true. */
function throttleBands(samples: CpuSample[]): [number, number][] {
  const bands: [number, number][] = [];
  let start: number | null = null;

  for (const s of samples) {
    const throttling = readingValue(s.thermal_throttling);
    if (throttling === true) {
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
