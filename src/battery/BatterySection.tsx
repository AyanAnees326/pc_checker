import { createPortal } from "react-dom";

import type { BatteryHistory, BatteryReport, Finding, Reading } from "../types";
import { readingValue } from "../types";
import {
  ComponentDetails,
  Empty,
  Field,
  FindingList,
  GraphsGroup,
  ScanButton,
  Section,
  Skeleton,
  StatusDot,
  formatDate,
  formatDuration,
  useComponentScan,
  worstSeverity,
} from "../ui";
import { MetricGraph } from "../ui/MetricGraph";

interface BatteryScanData {
  packs: Reading<BatteryReport[]>;
  history: Reading<BatteryHistory>;
}

export function BatterySection({ graphsContainer }: { graphsContainer: HTMLDivElement | null }) {
  const [state, run] = useComponentScan<BatteryScanData>("scan_battery");
  const severity = state.status === "done" ? worstSeverity(state.findings) : null;
  const history = state.status === "done" ? readingValue(state.data.history) : null;

  return (
    <>
      <Section
        title="Battery"
        subtitle="Health, wear, and capacity trend over time"
        action={<ScanButton status={state.status} onScan={run} label="battery" />}
        statusBadge={severity && <StatusDot severity={severity} />}
      >
        {state.status === "idle" && (
          <Empty>
            Reads battery registers directly (no driver, no elevation needed) and pulls
            Windows' own capacity history going back months.
          </Empty>
        )}
        {state.status === "loading" && <Skeleton />}
        {state.status === "error" && <p className="text-problem bg-problem/12 border border-problem/30 rounded-lg px-4 py-3">{state.message}</p>}
        {state.status === "done" && <BatteryBody data={state.data} findings={state.findings} scannedAt={state.scannedAt} />}
      </Section>

      {graphsContainer &&
        history &&
        history.samples.length > 0 &&
        createPortal(
          <GraphsGroup title="Battery — Capacity over time">
            <CapacityGraph history={history} />
          </GraphsGroup>,
          graphsContainer
        )}
    </>
  );
}

function BatteryBody({
  data,
  findings,
  scannedAt,
}: {
  data: BatteryScanData;
  findings: Finding[];
  scannedAt: string;
}) {
  const packs = readingValue(data.packs);
  const history = readingValue(data.history);

  if (!packs) {
    return <Empty>{data.packs.ok ? "" : data.packs.note}</Empty>;
  }

  return (
    <>
      <FindingList findings={findings} />

      <ComponentDetails>
        {packs.map((b, i) => (
          <div
            className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-x-6 gap-y-[0.15rem] mb-3 last:mb-0"
            key={i}
          >
            <Field label="Name" reading={b.device_name} />
            <Field label="Manufacturer" reading={b.manufacturer} />
            <Field label="Serial" reading={b.serial_number} />
            <Field label="Manufactured" reading={b.manufacture_date} />
            <Field label="Chemistry" reading={b.chemistry} />
            <Field label="Health" reading={b.health_percent} render={(v) => `${v.toFixed(1)}%`} />
            <Field label="Design capacity" reading={b.designed_capacity_mwh} suffix="mWh" />
            <Field label="Full charge capacity" reading={b.full_charged_capacity_mwh} suffix="mWh" />
            <Field label="Cycle count" reading={b.cycle_count} />
            <Field label="Voltage" reading={b.voltage_mv} suffix="mV" />
            <Field label="Charge / discharge" reading={b.charge_rate_mw} suffix="mW" />
            <Field label="Temperature" reading={b.temperature_c} render={(v) => v.toFixed(1)} suffix="°C" />
            <Field label="Charge" reading={b.current_capacity_percent} render={(v) => `${v.toFixed(0)}%`} />
            <Field label="Runtime left" reading={b.estimated_runtime_s} render={formatDuration} />
          </div>
        ))}
      </ComponentDetails>

      {history && history.samples.length > 0 && (
        <p className="text-muted text-sm mt-3">
          {history.samples.length} observations over {history.observation_days} days
          {history.degradation_percent_per_year.ok &&
            ` · trending ${history.degradation_percent_per_year.value.toFixed(1)}% per year`}
        </p>
      )}

      <p className="text-muted text-xs mt-3">Scanned {formatDate(scannedAt)}</p>
    </>
  );
}

/**
 * Capacity trend via the shared `MetricGraph`/`LiveChart` machinery instead of a
 * bespoke SVG — same equal-spacing-by-observation-index layout the old hand-rolled
 * chart used (period gaps aren't uniform, and index spacing reads more evenly than
 * date-proportional spacing would for sparse, irregular battery-report samples).
 */
function CapacityGraph({ history }: { history: BatteryHistory }) {
  if (history.samples.length < 2) return null;

  const elapsedMs = history.samples.map((_, i) => i);
  const values = history.samples.map((s) => s.health_percent);
  const markersMs = history.samples
    .map((s, i) => (s.battery_changed ? { ms: i, label: "Battery replaced" } : null))
    .filter((m): m is { ms: number; label: string } => m !== null);

  return (
    <MetricGraph
      label="Capacity health"
      colorClass="series"
      suffix="%"
      format={(v) => v.toFixed(1)}
      elapsedMs={elapsedMs}
      values={values}
      markersMs={markersMs}
    />
  );
}
