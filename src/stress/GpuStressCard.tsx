import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { createPortal } from "react-dom";

import type {
  Finding,
  GpuReport,
  GpuSample,
  GpuStressPhase,
  GpuStressResult,
  GpuStressStartedEvent,
  Reading,
} from "../types";
import { GPU_PHASE_LABEL, GPU_STRESS_PHASES, readingValue } from "../types";
import {
  ComponentDetails,
  DurationSelect,
  Empty,
  FindingList,
  GraphsGroup,
  ScanButton,
  Section,
  Stat,
  StatusDot,
  formatBytes,
  useComponentScan,
  worstSeverity,
} from "../ui";
import { MetricGraph } from "../ui/MetricGraph";
import { computeThrottleBands } from "./throttle";

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

const buttonClass =
  "btn-gradient rounded-lg px-3.5 py-1.5 text-[0.82rem] font-semibold whitespace-nowrap border-0 cursor-pointer";
const errorClass = "text-problem bg-problem/12 border border-problem/30 rounded-lg px-4 py-3";

export function GpuStressCard({ graphsContainer }: { graphsContainer: HTMLDivElement | null }) {
  const [state, setState] = useState<RunState>({ status: "idle" });
  const unlistenRefs = useRef<Array<() => void>>([]);
  const [durationMinutes, setDurationMinutes] = useState(11);
  const [identity, runIdentityScan] = useComponentScan<Reading<GpuReport[]>>("scan_gpu");

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
      await invoke("start_gpu_stress", { durationSecs: durationMinutes * 60 });
    } catch (e) {
      setState({ status: "error", message: String(e) });
      unlistenStarted();
      unlistenSample();
      unlistenComplete();
    }
  }, [durationMinutes]);

  const cancel = useCallback(() => {
    invoke("cancel_gpu_stress").catch(() => {});
  }, []);

  const samples = state.status === "running" ? state.samples : state.status === "done" ? state.result.samples : [];
  const gpu = state.status === "running" || state.status === "done" ? state.gpu : null;
  const latest = samples[samples.length - 1];
  const currentPhase: GpuStressPhase | null = latest?.phase ?? null;
  const currentPhaseIndex = currentPhase ? GPU_STRESS_PHASES.indexOf(currentPhase) : -1;
  const telemetryAvailable = latest ? readingValue(latest.graphics_clock_mhz) !== null : true;
  const elapsedMs = samples.map((s) => s.elapsed_ms);
  const throttleBandsMs = computeThrottleBands(
    samples,
    (s) =>
      readingValue(s.sw_thermal_slowdown) === true ||
      readingValue(s.hw_thermal_slowdown) === true ||
      readingValue(s.sw_power_cap) === true ||
      readingValue(s.hw_power_brake) === true
  );
  const identityList = identity.status === "done" ? readingValue(identity.data) : null;
  const gpuName = identityList?.find((g) => !g.is_software)?.name ?? identityList?.[0]?.name ?? null;
  const severity = worstSeverity([
    ...(identity.status === "done" ? identity.findings : []),
    ...(state.status === "done" ? state.findings : []),
  ]);

  return (
    <>
      <Section
        title="GPU"
        subtitle={gpuName ?? "Compute + raster pipeline stress, plus adapter identity"}
        statusBadge={severity && <StatusDot severity={severity} />}
        action={<ScanButton status={identity.status} onScan={runIdentityScan} label="adapter" />}
      >
      {identity.status === "idle" && (
        <Empty>Enumerates display adapters over DXGI. No load is applied.</Empty>
      )}
      {identity.status === "error" && <p className={errorClass}>{identity.message}</p>}
      {identity.status === "done" && (
        <ComponentDetails>
          {!identityList || identityList.length === 0 ? (
            <Empty>{identity.data.ok ? "No display adapters were enumerated." : identity.data.note}</Empty>
          ) : (
            identityList.map((g, i) => (
              <div key={i} className="flex items-center gap-3 py-1.5 border-b border-white/5 text-sm flex-wrap">
                <strong>{g.name}</strong>
                <span>
                  {g.vendor}
                  {g.is_software && " · software renderer"}
                </span>
                <span className="text-muted">
                  {g.dedicated_vram_bytes > 0 ? formatBytes(g.dedicated_vram_bytes) : "no dedicated VRAM"}
                </span>
                <span className="text-muted">driver {readingValue(g.driver_version) ?? "unknown"}</span>
              </div>
            ))
          )}
        </ComponentDetails>
      )}

      <div className="mt-3 pt-3 border-t border-border">
        <div className="flex items-center gap-2 flex-wrap">
          {state.status !== "running" && (
            <DurationSelect
              value={durationMinutes}
              onChange={setDurationMinutes}
              options={[3, 5, 11, 15, 20, 30]}
              defaultMinutes={11}
            />
          )}
          {state.status === "running" ? (
            <button className={buttonClass} onClick={cancel}>
              Cancel
            </button>
          ) : (
            <button className={buttonClass} onClick={start}>
              {state.status === "done" ? "Run again" : "Start GPU stress test"}
            </button>
          )}
        </div>

        {state.status === "idle" && (
          <Empty>
            Compute + raster pipeline load with a VRAM integrity check, self-checking every result —
            catches the corrupted-VRAM failure mode of ex-mining GPUs. Clock/power/temperature/fan
            need an NVIDIA (NVML) or AMD (ADL) GPU; Intel runs the stress and self-check without them.
          </Empty>
        )}

        {state.status === "error" && <p className={`${errorClass} mt-3`}>{state.message}</p>}

        {(state.status === "running" || state.status === "done") && (
          <>
            {gpu && (
              <p className="text-muted text-sm mt-3">
                {gpu.name} · {gpu.vendor}
                {!telemetryAvailable &&
                  " · clock/power/temperature/fan unavailable (vendor telemetry is NVIDIA/AMD-only in this build)"}
              </p>
            )}

            <div className="flex gap-1.5 mt-3 flex-wrap">
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
              <div className="flex gap-6 flex-wrap mt-3">
                <Stat
                  label="Self-check"
                  value={latest.self_check_ok === null ? null : latest.self_check_ok ? "OK" : "FAILED"}
                />
                <Stat
                  label="Dispatches"
                  value={latest.dispatches_completed}
                  format={(v) => Math.round(v).toLocaleString()}
                />
              </div>
            )}

            {state.status === "done" && (
              <>
                {state.result.aborted && (
                  <p className="mt-3 italic text-muted text-sm">
                    Run stopped early{state.result.abort_reason ? `: ${state.result.abort_reason}` : "."}
                  </p>
                )}
                <div className="mt-4">
                  <FindingList findings={state.findings} />
                </div>
              </>
            )}
          </>
        )}
      </div>
      </Section>

      {graphsContainer &&
        latest &&
        (state.status === "running" || state.status === "done") &&
        createPortal(
          <GraphsGroup title="GPU — Stress test">
            <MetricGraph
              label="Clock"
              colorClass="series"
              unit="MHz"
              suffix="MHz"
              elapsedMs={elapsedMs}
              values={samples.map((s) => readingValue(s.graphics_clock_mhz))}
              throttleBandsMs={throttleBandsMs}
            />
            <MetricGraph
              label="Power"
              colorClass="series-2"
              unit="W"
              suffix="W"
              elapsedMs={elapsedMs}
              values={samples.map((s) => readingValue(s.power_watts))}
              throttleBandsMs={throttleBandsMs}
            />
            <MetricGraph
              label="Edge temp"
              colorClass="series-3"
              unit="°C"
              suffix="°C"
              elapsedMs={elapsedMs}
              values={samples.map((s) => readingValue(s.edge_temperature_c))}
              throttleBandsMs={throttleBandsMs}
            />
            <MetricGraph
              label="Hotspot temp"
              colorClass="series-4"
              unit="°C"
              suffix="°C"
              elapsedMs={elapsedMs}
              values={samples.map((s) => readingValue(s.hotspot_temperature_c))}
              throttleBandsMs={throttleBandsMs}
            />
            <MetricGraph
              label="Fan"
              colorClass="series"
              unit="RPM"
              suffix="RPM"
              elapsedMs={elapsedMs}
              values={samples.map((s) => readingValue(s.fan_rpm))}
            />
            <MetricGraph
              label="Fan speed"
              colorClass="series-2"
              unit="%"
              suffix="%"
              elapsedMs={elapsedMs}
              values={samples.map((s) => readingValue(s.fan_percent))}
            />
          </GraphsGroup>,
          graphsContainer
        )}
    </>
  );
}
