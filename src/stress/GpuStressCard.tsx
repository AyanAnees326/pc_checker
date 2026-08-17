import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

import type {
  Finding,
  GpuReport,
  GpuSample,
  GpuStressPhase,
  GpuStressResult,
  GpuStressStartedEvent,
} from "../types";
import { GPU_PHASE_LABEL, GPU_STRESS_PHASES, readingValue } from "../types";
import { Empty, FindingList, Section } from "../ui";
import { LiveChart } from "./LiveChart";

type RunState =
  | { status: "idle" }
  | { status: "running"; gpu: GpuReport | null; samples: GpuSample[] }
  | { status: "done"; gpu: GpuReport | null; result: GpuStressResult; findings: Finding[] }
  | { status: "error"; message: string };

interface CompletePayload {
  data: GpuStressResult;
  findings: Finding[];
  scanned_at: string;
}

export function GpuStressCard() {
  const [state, setState] = useState<RunState>({ status: "idle" });
  const unlistenRefs = useRef<Array<() => void>>([]);

  useEffect(() => {
    return () => {
      unlistenRefs.current.forEach((u) => u());
    };
  }, []);

  const start = useCallback(async () => {
    setState({ status: "running", gpu: null, samples: [] });

    const unlistenStarted = await listen<GpuStressStartedEvent>("stress://gpu/started", (e) => {
      setState((prev) => (prev.status === "running" ? { ...prev, gpu: e.payload.gpu } : prev));
    });

    const unlistenSample = await listen<GpuSample>("stress://gpu/sample", (e) => {
      setState((prev) =>
        prev.status === "running" ? { ...prev, samples: [...prev.samples, e.payload] } : prev
      );
    });

    const unlistenComplete = await listen<CompletePayload>("stress://gpu/complete", (e) => {
      setState((prev) => ({
        status: "done",
        gpu: prev.status === "running" ? prev.gpu : null,
        result: e.payload.data,
        findings: e.payload.findings,
      }));
      unlistenStarted();
      unlistenSample();
      unlistenComplete();
    });

    unlistenRefs.current = [unlistenStarted, unlistenSample, unlistenComplete];

    try {
      await invoke("start_gpu_stress");
    } catch (e) {
      setState({ status: "error", message: String(e) });
      unlistenStarted();
      unlistenSample();
      unlistenComplete();
    }
  }, []);

  const cancel = useCallback(() => {
    invoke("cancel_gpu_stress").catch(() => {});
  }, []);

  const samples = state.status === "running" ? state.samples : state.status === "done" ? state.result.samples : [];
  const gpu = state.status === "running" || state.status === "done" ? state.gpu : null;
  const latest = samples[samples.length - 1];
  const currentPhase: GpuStressPhase | null = latest?.phase ?? null;
  const currentPhaseIndex = currentPhase ? GPU_STRESS_PHASES.indexOf(currentPhase) : -1;
  const telemetryAvailable = latest ? readingValue(latest.graphics_clock_mhz) !== null : true;

  return (
    <Section
      title="GPU stress test"
      subtitle="~10 minutes — compute load plus a VRAM integrity check built into the same self-check"
      action={
        state.status === "running" ? (
          <button className="scan-btn" onClick={cancel}>
            Cancel
          </button>
        ) : (
          <button className="scan-btn" onClick={start}>
            {state.status === "done" ? "Run again" : "Start GPU stress test"}
          </button>
        )
      }
    >
      {state.status === "idle" && (
        <Empty>
          Runs a compute shader against a large VRAM-resident buffer, self-checking every result — a
          corrupted VRAM cell (the classic ex-mining-GPU failure mode) is caught the same way a compute
          fault would be. Clock, power, temperature and fan readings need an NVIDIA (NVML) or AMD (ADL)
          GPU; Intel runs the same stress and self-check without those readings.
        </Empty>
      )}

      {state.status === "error" && <p className="error">{state.message}</p>}

      {(state.status === "running" || state.status === "done") && (
        <>
          {gpu && (
            <p className="muted" style={{ marginBottom: "0.75rem" }}>
              {gpu.name} · {gpu.vendor}
              {!telemetryAvailable &&
                " · clock/power/temperature/fan unavailable (vendor telemetry is NVIDIA/AMD-only in this build)"}
            </p>
          )}

          <div className="phase-strip">
            {GPU_STRESS_PHASES.map((phase, i) => (
              <span
                key={phase}
                className={
                  "phase-chip" +
                  (i === currentPhaseIndex ? " active" : i < currentPhaseIndex ? " done" : "")
                }
              >
                {GPU_PHASE_LABEL[phase]}
              </span>
            ))}
          </div>

          {latest && (
            <div className="stat-row">
              <Stat label="Clock" value={readingValue(latest.graphics_clock_mhz)} suffix=" MHz" />
              <Stat label="Power" value={readingValue(latest.power_watts)} suffix=" W" />
              <Stat label="Edge temp" value={readingValue(latest.edge_temperature_c)} suffix=" °C" />
              <Stat label="Hotspot temp" value={readingValue(latest.hotspot_temperature_c)} suffix=" °C" />
              <Stat label="Fan" value={readingValue(latest.fan_rpm)} suffix=" RPM" />
              <Stat label="Fan speed" value={readingValue(latest.fan_percent)} suffix="%" />
              <Stat
                label="Self-check"
                value={latest.self_check_ok === null ? null : latest.self_check_ok ? "OK" : "FAILED"}
              />
              <Stat label="Dispatches" value={latest.dispatches_completed.toLocaleString()} />
            </div>
          )}

          {samples.length > 1 && (
            <LiveChart
              elapsedMs={samples.map((s) => s.elapsed_ms)}
              throttleBandsMs={throttleBands(samples)}
              series={[
                {
                  label: "Clock",
                  colorClass: "series",
                  unit: "MHz",
                  values: samples.map((s) => readingValue(s.graphics_clock_mhz)),
                },
                {
                  label: "Power",
                  colorClass: "series-2",
                  unit: "W",
                  values: samples.map((s) => readingValue(s.power_watts)),
                },
                {
                  label: "Edge temp",
                  colorClass: "series-3",
                  unit: "°C",
                  values: samples.map((s) => readingValue(s.edge_temperature_c)),
                },
                {
                  label: "Hotspot temp",
                  colorClass: "series-4",
                  unit: "°C",
                  values: samples.map((s) => readingValue(s.hotspot_temperature_c)),
                },
              ]}
            />
          )}

          {state.status === "done" && (
            <>
              {state.result.aborted && (
                <p className="note">
                  Run stopped early{state.result.abort_reason ? `: ${state.result.abort_reason}` : "."}
                </p>
              )}
              <div style={{ marginTop: "1rem" }}>
                <FindingList findings={state.findings} />
              </div>
            </>
          )}
        </>
      )}
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

/** Contiguous [startMs, endMs] ranges where any throttle-reason bit read true. */
function throttleBands(samples: GpuSample[]): [number, number][] {
  const bands: [number, number][] = [];
  let start: number | null = null;

  for (const s of samples) {
    const throttling =
      readingValue(s.sw_thermal_slowdown) === true ||
      readingValue(s.hw_thermal_slowdown) === true ||
      readingValue(s.sw_power_cap) === true ||
      readingValue(s.hw_power_brake) === true;
    if (throttling) {
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
