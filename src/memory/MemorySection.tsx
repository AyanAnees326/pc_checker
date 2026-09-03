import type { Finding, MemoryReport } from "../types";
import { readingValue } from "../types";
import { ComponentDetails, Empty, FindingList, Plain, ScanButton, Section, Skeleton, StatusDot, formatDate, useComponentScan, worstSeverity } from "../ui";

export function MemorySection() {
  const [state, run] = useComponentScan<MemoryReport>("scan_memory");
  const severity = state.status === "done" ? worstSeverity(state.findings) : null;

  return (
    <Section
      title="Memory"
      subtitle="Configuration only — not the destructive pattern test"
      action={<ScanButton status={state.status} onScan={run} label="memory" />}
      statusBadge={severity && <StatusDot severity={severity} />}
    >
      {state.status === "idle" && (
        <Empty>Reads channel population, rated vs configured speed, and upgrade headroom from SMBIOS.</Empty>
      )}
      {state.status === "loading" && <Skeleton />}
      {state.status === "error" && <p className="text-problem bg-problem/12 border border-problem/30 rounded-lg px-4 py-3">{state.message}</p>}
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

      <div className="grid grid-cols-[repeat(auto-fill,minmax(220px,1fr))] gap-x-6 gap-y-[0.15rem] mb-4">
        <Plain label="Total">{(report.total_mb / 1024).toFixed(0)} GB</Plain>
        <Plain label="Slots">
          {report.populated_slots} of {report.total_slots} populated
        </Plain>
        <Plain label="Channels">{report.channel_config}</Plain>
        <Plain label="Upgradeable">{report.all_soldered ? "No — soldered" : "Yes"}</Plain>
      </div>

      <ComponentDetails label="Module details">
        {report.modules.map((m, i) => (
          <div className="flex items-center gap-3 py-1.5 border-b border-white/5 text-sm flex-wrap" key={i}>
            <strong>{readingValue(m.slot) ?? `Module ${i + 1}`}</strong>
            <span>
              {(m.size_mb / 1024).toFixed(0)} GB {m.memory_type} · {m.form_factor}
            </span>
            <span className="text-muted">
              {readingValue(m.configured_speed_mts) ?? "?"} MT/s
              {readingValue(m.rated_speed_mts) !== readingValue(m.configured_speed_mts) &&
                ` (rated ${readingValue(m.rated_speed_mts) ?? "?"})`}
            </span>
            <span className="text-muted">
              {readingValue(m.manufacturer) ?? ""} {readingValue(m.part_number) ?? ""}
            </span>
          </div>
        ))}
      </ComponentDetails>

      <p className="text-muted text-xs mt-3">Scanned {formatDate(scannedAt)}</p>
    </>
  );
}
