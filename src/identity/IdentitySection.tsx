import type { Finding, FirmwareReport } from "../types";
import { readingValue } from "../types";
import { Empty, Field, FindingList, Plain, ScanButton, Section, Skeleton, StatusDot, formatDate, useComponentScan, worstSeverity } from "../ui";

export function IdentitySection() {
  const [state, run] = useComponentScan<FirmwareReport>("scan_firmware");
  const severity = state.status === "done" ? worstSeverity(state.findings) : null;

  return (
    <Section
      title="Identity & firmware"
      subtitle="Machine identity, and what survives wiping the disk"
      action={<ScanButton status={state.status} onScan={run} label="identity" />}
      statusBadge={severity && <StatusDot severity={severity} />}
    >
      {state.status === "idle" && (
        <Empty>
          Reads SMBIOS (model, serial, BIOS) and the live ACPI table list. Nothing is
          written to the machine.
        </Empty>
      )}
      {state.status === "loading" && <Skeleton />}
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
