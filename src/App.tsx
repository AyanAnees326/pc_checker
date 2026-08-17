import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type {
  BatteryHistory,
  BatteryReport,
  CrashHistory,
  DriveReport,
  Finding,
  FirmwareReport,
  GpuReport,
  MemoryReport,
  Reading,
} from "./types";
import { readingValue } from "./types";
import {
  Empty,
  Field,
  FindingList,
  Plain,
  ScanButton,
  Section,
  formatBytes,
  formatDate,
  formatDuration,
  formatHours,
  useComponentScan,
} from "./ui";
import { CpuSection } from "./cpu/CpuSection";
import { GpuStressCard } from "./stress/GpuStressCard";

export default function App() {
  const [elevated, setElevated] = useState<boolean | null>(null);

  // Elevation is a cheap OS query, not a hardware probe, so it is fetched once on
  // load rather than gated behind its own scan button.
  useEffect(() => {
    invoke<boolean>("is_elevated")
      .then(setElevated)
      .catch(() => setElevated(null));
  }, []);

  return (
    <main className="app">
      <header className="app-head">
        <div>
          <h1>PC Checker</h1>
          <p className="tagline">
            Inspect a used machine before you pay for it — scan each part on its own.
          </p>
        </div>
        {elevated === false && (
          <span
            className="badge badge-watch elevation-badge"
            title="Relaunch as administrator to unlock CPU/GPU sensor readings during stress tests"
          >
            Not elevated
          </span>
        )}
      </header>

      <div className="cards">
        <div className="card-wide">
          <CpuSection />
        </div>
        <div className="card-wide">
          <GpuStressCard />
        </div>
        <IdentitySection />
        <BatterySection />
        <StorageSection />
        <MemorySection />
        <GraphicsSection />
        <StabilitySection />
      </div>
    </main>
  );
}

// ---------------------------------------------------------------------------
// Identity & firmware
// ---------------------------------------------------------------------------

function IdentitySection() {
  const [state, run] = useComponentScan<FirmwareReport>("scan_firmware");

  return (
    <Section
      title="Identity & firmware"
      subtitle="Machine identity, and what survives wiping the disk"
      action={<ScanButton status={state.status} onScan={run} label="identity" />}
    >
      {state.status === "idle" && (
        <Empty>
          Reads SMBIOS (model, serial, BIOS) and the live ACPI table list. Nothing is
          written to the machine.
        </Empty>
      )}
      {state.status === "error" && <p className="error">{state.message}</p>}
      {state.status === "done" && <IdentityBody report={state.data} findings={state.findings} scannedAt={state.scannedAt} />}
    </Section>
  );
}

function IdentityBody({
  report,
  findings,
  scannedAt,
}: {
  report: FirmwareReport;
  findings: Finding[];
  scannedAt: string;
}) {
  const id = report.identity;
  const model =
    readingValue(id.product_name) ?? readingValue(id.baseboard_product) ?? "Unknown model";
  const maker = readingValue(id.manufacturer) ?? "";

  return (
    <>
      <div className="machine">
        <div>
          <h2 className="machine-name">
            {maker} {model}
          </h2>
          <p className="machine-meta">
            {id.form_factor.replace(/_/g, " ")} · BIOS {readingValue(id.bios_version) ?? "?"} (
            {readingValue(id.bios_release_date) ?? "date unknown"})
          </p>
        </div>
      </div>

      <FindingList findings={findings} />

      <div className="grid">
        <Field label="Manufacturer" reading={id.manufacturer} />
        <Field label="Model" reading={id.product_name} />
        <Field label="Serial number" reading={id.serial_number} />
        <Field label="SKU" reading={id.sku} />
        <Field label="Family" reading={id.family} />
        <Field label="Motherboard" reading={id.baseboard_product} />
        <Field label="BIOS vendor" reading={id.bios_vendor} />
        <Field label="BIOS version" reading={id.bios_version} />
        <Field label="BIOS date" reading={id.bios_release_date} />
        <Field label="SMBIOS" reading={id.smbios_version} />
      </div>

      <div className="grid" style={{ marginTop: "0.75rem" }}>
        <Plain label="WPBT in ACPI">
          {report.persistence.wpbt_present ? "Present" : "Absent"}
        </Plain>
        <Plain label="WPBT launcher on disk">
          {report.persistence.wpbt_launcher_present ? "Present" : "Absent"}
        </Plain>
        <Plain label="Absolute / Computrace">
          {report.persistence.absolute_agent_artifacts.length > 0
            ? report.persistence.absolute_agent_artifacts.join(", ")
            : "Not detected"}
        </Plain>
        <Plain label="ACPI tables">{report.persistence.acpi_tables.length}</Plain>
      </div>

      <p className="scan-time">Scanned {formatDate(scannedAt)}</p>
    </>
  );
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

interface BatteryScanData {
  packs: Reading<BatteryReport[]>;
  history: Reading<BatteryHistory>;
}

function BatterySection() {
  const [state, run] = useComponentScan<BatteryScanData>("scan_battery");

  return (
    <Section
      title="Battery"
      subtitle="Health, wear, and capacity trend over time"
      action={<ScanButton status={state.status} onScan={run} label="battery" />}
    >
      {state.status === "idle" && (
        <Empty>
          Reads battery registers directly (no driver, no elevation needed) and pulls
          Windows' own capacity history going back months.
        </Empty>
      )}
      {state.status === "error" && <p className="error">{state.message}</p>}
      {state.status === "done" && <BatteryBody data={state.data} findings={state.findings} scannedAt={state.scannedAt} />}
    </Section>
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

      {packs.map((b, i) => (
        <div className="grid" key={i}>
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

      {history && history.samples.length > 0 && (
        <div className="history">
          <h4>Capacity over time</h4>
          <p className="muted">
            {history.samples.length} observations over {history.observation_days} days
            {history.degradation_percent_per_year.ok &&
              ` · trending ${history.degradation_percent_per_year.value.toFixed(1)}% per year`}
          </p>
          <CapacityChart history={history} />
        </div>
      )}

      <p className="scan-time">Scanned {formatDate(scannedAt)}</p>
    </>
  );
}

/**
 * Capacity trend as an inline SVG.
 *
 * A single health percentage is ambiguous — 85% falling fast and 85% stable for two
 * years are opposite conclusions. The shape of this line is the actual finding.
 */
function CapacityChart({ history }: { history: BatteryHistory }) {
  if (history.samples.length < 2) return null;

  const w = 640;
  const h = 160;
  const pad = { top: 12, right: 12, bottom: 24, left: 40 };

  const values = history.samples.map((s) => s.health_percent);
  const min = Math.max(0, Math.min(...values) - 5);
  const max = Math.min(105, Math.max(...values) + 5);

  const x = (i: number) =>
    pad.left + (i / (history.samples.length - 1)) * (w - pad.left - pad.right);
  const y = (v: number) =>
    pad.top + (1 - (v - min) / (max - min || 1)) * (h - pad.top - pad.bottom);

  const path = history.samples
    .map((s, i) => `${i === 0 ? "M" : "L"} ${x(i).toFixed(1)} ${y(s.health_percent).toFixed(1)}`)
    .join(" ");

  return (
    <svg viewBox={`0 0 ${w} ${h}`} className="chart" role="img" aria-label="Battery capacity over time">
      <line x1={pad.left} y1={h - pad.bottom} x2={w - pad.right} y2={h - pad.bottom} className="axis" />
      <line x1={pad.left} y1={pad.top} x2={pad.left} y2={h - pad.bottom} className="axis" />
      <text x={4} y={y(max) + 4} className="tick">{max.toFixed(0)}%</text>
      <text x={4} y={y(min) + 4} className="tick">{min.toFixed(0)}%</text>
      <path d={path} className="series" />
      {history.samples.map(
        (s, i) =>
          s.battery_changed && (
            <line key={i} x1={x(i)} y1={pad.top} x2={x(i)} y2={h - pad.bottom} className="swap">
              <title>Battery replaced</title>
            </line>
          )
      )}
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

function StorageSection() {
  const [state, run] = useComponentScan<DriveReport[]>("scan_storage");

  return (
    <Section
      title="Storage"
      subtitle="Power-on hours is the machine's odometer"
      action={<ScanButton status={state.status} onScan={run} label="storage" />}
    >
      {state.status === "idle" && (
        <Empty>
          Reads SMART/health data from every physical drive over NVMe or ATA. Opened
          with zero access rights — this scan cannot modify a drive.
        </Empty>
      )}
      {state.status === "error" && <p className="error">{state.message}</p>}
      {state.status === "done" && (
        <>
          <FindingList findings={state.findings} />
          {state.data.length === 0 ? (
            <Empty>No physical drives were enumerated.</Empty>
          ) : (
            state.data.map((d) => <Drive key={d.index} drive={d} />)
          )}
          <p className="scan-time">Scanned {formatDate(state.scannedAt)}</p>
        </>
      )}
    </Section>
  );
}

function Drive({ drive }: { drive: DriveReport }) {
  const health = readingValue(drive.health);

  return (
    <div className="drive">
      <div className="drive-head">
        <strong>{readingValue(drive.model) ?? `Drive ${drive.index}`}</strong>
        <span className="muted">{drive.bus_type}</span>
      </div>

      {!health ? (
        <p className="missing">{drive.health.ok ? "" : drive.health.note}</p>
      ) : health.protocol === "ata" ? (
        <div className="grid">
          <Field label="Power-on hours" reading={health.power_on_hours} render={formatHours} />
          <Field label="Power cycles" reading={health.power_cycles} render={(v) => v.toLocaleString()} />
          <Field label="Reallocated sectors" reading={health.reallocated_sectors} />
          <Field label="Pending sectors" reading={health.pending_sectors} />
          <Field label="Uncorrectable sectors" reading={health.uncorrectable_sectors} />
          <Field label="Temperature" reading={health.temperature_c} suffix="°C" />
          <Field label="Endurance left" reading={health.life_remaining_percent} suffix="%" />
          <Field label="Written" reading={health.terabytes_written} render={(v) => `${v.toFixed(2)} TB`} />
        </div>
      ) : (
        <div className="grid">
          <Field label="Power-on hours" reading={health.power_on_hours} render={formatHours} />
          <Field label="Power cycles" reading={health.power_cycles} render={(v) => v.toLocaleString()} />
          <Field label="Endurance used" reading={health.percentage_used} suffix="%" />
          <Field label="Spare blocks" reading={health.available_spare_percent} suffix="%" />
          <Field label="Media errors" reading={health.media_errors} />
          <Field label="Unsafe shutdowns" reading={health.unsafe_shutdowns} />
          <Field label="Temperature" reading={health.composite_temp_c} render={(v) => v.toFixed(0)} suffix="°C" />
          <Field label="Written" reading={health.terabytes_written} render={(v) => `${v.toFixed(2)} TB`} />
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

function MemorySection() {
  const [state, run] = useComponentScan<MemoryReport>("scan_memory");

  return (
    <Section
      title="Memory"
      subtitle="Configuration only — not the destructive pattern test"
      action={<ScanButton status={state.status} onScan={run} label="memory" />}
    >
      {state.status === "idle" && (
        <Empty>Reads channel population, rated vs configured speed, and upgrade headroom from SMBIOS.</Empty>
      )}
      {state.status === "error" && <p className="error">{state.message}</p>}
      {state.status === "done" && <MemoryBody report={state.data} findings={state.findings} scannedAt={state.scannedAt} />}
    </Section>
  );
}

function MemoryBody({
  report,
  findings,
  scannedAt,
}: {
  report: MemoryReport;
  findings: Finding[];
  scannedAt: string;
}) {
  return (
    <>
      <FindingList findings={findings} />

      <div className="grid">
        <Plain label="Total">{(report.total_mb / 1024).toFixed(0)} GB</Plain>
        <Plain label="Slots">
          {report.populated_slots} of {report.total_slots} populated
        </Plain>
        <Plain label="Channels">{report.channel_config}</Plain>
        <Plain label="Upgradeable">{report.all_soldered ? "No — soldered" : "Yes"}</Plain>
      </div>

      {report.modules.map((m, i) => (
        <div className="row" key={i}>
          <strong>{readingValue(m.slot) ?? `Module ${i + 1}`}</strong>
          <span>
            {(m.size_mb / 1024).toFixed(0)} GB {m.memory_type} · {m.form_factor}
          </span>
          <span className="muted">
            {readingValue(m.configured_speed_mts) ?? "?"} MT/s
            {readingValue(m.rated_speed_mts) !== readingValue(m.configured_speed_mts) &&
              ` (rated ${readingValue(m.rated_speed_mts) ?? "?"})`}
          </span>
          <span className="muted">
            {readingValue(m.manufacturer) ?? ""} {readingValue(m.part_number) ?? ""}
          </span>
        </div>
      ))}

      <p className="scan-time">Scanned {formatDate(scannedAt)}</p>
    </>
  );
}

// ---------------------------------------------------------------------------
// Graphics
// ---------------------------------------------------------------------------

function GraphicsSection() {
  const [state, run] = useComponentScan<Reading<GpuReport[]>>("scan_gpu");

  return (
    <Section
      title="Graphics"
      subtitle="Adapter identity — temperature, clocks and power come from the stress test"
      action={<ScanButton status={state.status} onScan={run} label="graphics" />}
    >
      {state.status === "idle" && <Empty>Enumerates display adapters over DXGI. No load is applied.</Empty>}
      {state.status === "error" && <p className="error">{state.message}</p>}
      {state.status === "done" && (
        <>
          <FindingList findings={state.findings} />
          <GraphicsBody gpus={state.data} />
          <p className="scan-time">Scanned {formatDate(state.scannedAt)}</p>
        </>
      )}
    </Section>
  );
}

function GraphicsBody({ gpus }: { gpus: Reading<GpuReport[]> }) {
  const list = readingValue(gpus);
  if (!list || list.length === 0) {
    return <Empty>{gpus.ok ? "No display adapters were enumerated." : gpus.note}</Empty>;
  }

  return (
    <>
      {list.map((g, i) => (
        <div className="row" key={i}>
          <strong>{g.name}</strong>
          <span>
            {g.vendor}
            {g.is_software && " · software renderer"}
          </span>
          <span className="muted">
            {g.dedicated_vram_bytes > 0 ? formatBytes(g.dedicated_vram_bytes) : "no dedicated VRAM"}
          </span>
          <span className="muted">driver {readingValue(g.driver_version) ?? "unknown"}</span>
        </div>
      ))}
    </>
  );
}

// ---------------------------------------------------------------------------
// Stability
// ---------------------------------------------------------------------------

function StabilitySection() {
  const [state, run] = useComponentScan<CrashHistory>("scan_stability");

  return (
    <Section
      title="Stability"
      subtitle="History a reinstall of the user profile does not erase"
      action={<ScanButton status={state.status} onScan={run} label="stability" />}
    >
      {state.status === "idle" && (
        <Empty>Reads crash dump files and WHEA/power-loss events from the Windows event log.</Empty>
      )}
      {state.status === "error" && <p className="error">{state.message}</p>}
      {state.status === "done" && (
        <>
          <FindingList findings={state.findings} />
          <div className="grid">
            <Plain label="Blue screens recorded">{state.data.minidump_count}</Plain>
            <Plain label="In the last 30 days">{state.data.minidumps_last_30_days}</Plain>
            <Field label="Most recent crash" reading={state.data.most_recent_minidump} render={formatDate} />
            <Plain label="Hardware errors (WHEA)">
              {readingValue(state.data.whea_events)?.length ?? "unavailable"}
              {state.data.whea_uncorrected_count > 0 && ` · ${state.data.whea_uncorrected_count} error-level`}
            </Plain>
            <Plain label="Unexpected shutdowns">
              {readingValue(state.data.unexpected_shutdowns)?.length ?? "unavailable"}
            </Plain>
          </div>
          <p className="scan-time">Scanned {formatDate(state.scannedAt)}</p>
        </>
      )}
    </Section>
  );
}
