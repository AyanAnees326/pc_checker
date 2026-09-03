import type { CrashHistory } from "../types";
import { readingValue } from "../types";
import { Empty, Field, FindingList, Plain, ScanButton, Section, Skeleton, StatusDot, formatDate, useComponentScan, worstSeverity } from "../ui";

export function StabilitySection() {
  const [state, run] = useComponentScan<CrashHistory>("scan_stability");
  const severity = state.status === "done" ? worstSeverity(state.findings) : null;

  return (
    <Section
      title="Stability"
      subtitle="History a reinstall of the user profile does not erase"
      action={<ScanButton status={state.status} onScan={run} label="stability" />}
      statusBadge={severity && <StatusDot severity={severity} />}
    >
      {state.status === "idle" && (
        <Empty>Reads crash dump files and WHEA/power-loss events from the Windows event log.</Empty>
      )}
      {state.status === "loading" && <Skeleton />}
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
