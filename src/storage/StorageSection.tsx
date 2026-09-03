import type { DriveReport } from "../types";
import { readingValue } from "../types";
import { ComponentDetails, Empty, Field, FindingList, ScanButton, Section, Skeleton, StatusDot, formatDate, formatHours, useComponentScan, worstSeverity } from "../ui";

export function StorageSection() {
  const [state, run] = useComponentScan<DriveReport[]>("scan_storage");
  const severity = state.status === "done" ? worstSeverity(state.findings) : null;

  return (
    <Section
      title="Storage"
      subtitle="Power-on hours is the machine's odometer"
      action={<ScanButton status={state.status} onScan={run} label="storage" />}
      statusBadge={severity && <StatusDot severity={severity} />}
    >
      {state.status === "idle" && (
        <Empty>
          Reads SMART/health data from every physical drive over NVMe or ATA. Opened
          with zero access rights — this scan cannot modify a drive.
        </Empty>
      )}
      {state.status === "loading" && <Skeleton />}
      {state.status === "error" && <p className="text-problem bg-problem/12 border border-problem/30 rounded-lg px-4 py-3">{state.message}</p>}
      {state.status === "done" && (
        <>
          <FindingList findings={state.findings} />
          {state.data.length === 0 ? (
            <Empty>No physical drives were enumerated.</Empty>
          ) : (
            state.data.map((d) => <Drive key={d.index} drive={d} />)
          )}
          <p className="text-muted text-xs mt-3">Scanned {formatDate(state.scannedAt)}</p>
        </>
      )}
    </Section>
  );
}

function Drive({ drive }: { drive: DriveReport }) {
  const health = readingValue(drive.health);

  return (
    <div className="glass-panel rounded-lg p-4 mb-3">
      <div className="flex items-center justify-between gap-2 mb-2 flex-wrap">
        <strong>{readingValue(drive.model) ?? `Drive ${drive.index}`}</strong>
        <span className="text-muted text-sm">{drive.bus_type}</span>
      </div>

      {!health ? (
        <p className="missing text-sm">{drive.health.ok ? "" : drive.health.note}</p>
      ) : (
        <ComponentDetails>
          {health.protocol === "ata" ? (
            <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-x-6 gap-y-[0.15rem]">
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
            <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-x-6 gap-y-[0.15rem]">
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
        </ComponentDetails>
      )}
    </div>
  );
}
