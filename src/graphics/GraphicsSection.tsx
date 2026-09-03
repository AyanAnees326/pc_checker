import type { GpuReport, Reading } from "../types";
import { readingValue } from "../types";
import { Empty, FindingList, ScanButton, Section, Skeleton, StatusDot, formatBytes, formatDate, useComponentScan, worstSeverity } from "../ui";

export function GraphicsSection() {
  const [state, run] = useComponentScan<Reading<GpuReport[]>>("scan_gpu");
  const severity = state.status === "done" ? worstSeverity(state.findings) : null;

  return (
    <Section
      title="Graphics"
      subtitle="Adapter identity — temperature, clocks and power come from the stress test"
      action={<ScanButton status={state.status} onScan={run} label="graphics" />}
      statusBadge={severity && <StatusDot severity={severity} />}
    >
      {state.status === "idle" && <Empty>Enumerates display adapters over DXGI. No load is applied.</Empty>}
      {state.status === "loading" && <Skeleton />}
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
